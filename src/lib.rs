//! A crate for installing Ubuntu distributions from a live squashfs

#![allow(unknown_lints)]

#[macro_use]
extern crate derive_new;
extern crate failure;
#[macro_use]
extern crate failure_derive;
extern crate fern;
extern crate itertools;
#[macro_use]
extern crate lazy_static;
extern crate libc;
extern crate libparted;
#[macro_use]
extern crate log;
extern crate gettextrs;
extern crate iso3166_1;
extern crate isolang;
extern crate rand;
extern crate rayon;
extern crate raw_cpuid;
extern crate tempdir;
#[macro_use]
extern crate serde_derive;
extern crate serde_xml_rs;

use disk::external::{blockdev, cryptsetup_close, dmlist, encrypted_devices, pvs, remount_rw, vgactivate, vgdeactivate, CloseBy};
use disk::operations::FormatPartitions;
use itertools::Itertools;
use os_release::OS_RELEASE;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Permissions};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, ATOMIC_BOOL_INIT, ATOMIC_USIZE_INIT};
use std::thread::sleep;
use std::time::Duration;
use tempdir::TempDir;

pub use chroot::Chroot;
pub use disk::mount::{Mount, Mounts};
pub use disk::{
    generate_unique_id, Bootloader, DecryptionError, Disk, DiskError, DiskExt, Disks,
    FileSystemType, LvmDevice, LvmEncryption, PartitionBuilder, PartitionError, PartitionFlag,
    PartitionInfo, PartitionTable, PartitionType, Sector, OS,
};
pub use misc::device_layout_hash;

pub mod auto;
mod chroot;
mod disk;
mod envfile;
mod hardware_support;
pub mod hostname;
pub mod locale;
mod misc;
pub mod os_release;
mod squashfs;

use auto::{validate_before_removing, AccountFiles, Backup, ReinstallError};
use envfile::EnvFile;
use log::LevelFilter;

/// When set to true, this will stop the installation process.
pub static KILL_SWITCH: AtomicBool = ATOMIC_BOOL_INIT;

/// Force the installation to perform either a BIOS or EFI installation.
pub static FORCE_BOOTLOADER: AtomicUsize = ATOMIC_USIZE_INIT;

/// Exits before the unsquashfs step
pub static PARTITIONING_TEST: AtomicBool = ATOMIC_BOOL_INIT;

/// Self-explanatory -- the fstab file will be generated with this header.
const FSTAB_HEADER: &[u8] = b"# /etc/fstab: static file system information.
#
# Use 'blkid' to print the universally unique identifier for a
# device; this may be used with UUID= as a more robust way to name devices
# that works even if disks are added and removed. See fstab(5).
#
# <file system>  <mount point>  <type>  <options>  <dump>  <pass>
";

pub const DEFAULT_ESP_SECTORS: u64 = 1_024_000;
pub const DEFAULT_RECOVER_SECTORS: u64 = 8_388_608;
pub const DEFAULT_SWAP_SECTORS: u64 = DEFAULT_RECOVER_SECTORS;

macro_rules! file_create {
    ($path:expr, $perm:expr, [ $($data:expr),+ ]) => {{
        let mut file = File::create($path)?;
        $(file.write_all($data)?;)+
        file.set_permissions(Permissions::from_mode($perm))?;
        file.sync_all()?;
    }};

    ($path:expr, [ $($data:expr),+ ]) => {{
        let mut file = File::create($path)?;
        $(file.write_all($data)?;)+
        file.sync_all()?;
    }};
}

/// Initialize logging with the fern logger
pub fn log<F: Fn(log::Level, &str) + Send + Sync + 'static>(
    callback: F,
) -> Result<(), fern::InitError> {
    fern::Dispatch::new()
        // Exclude logs for crates that we use
        .level(LevelFilter::Off)
        // Include only the logs for this binary
        .level_for("distinst", LevelFilter::Debug)
        // This will be used by the front end for display logs in a UI
        .chain(fern::Output::call(move |record| {
            callback(record.level(), &format!("{}", record.args()))
        }))
        // Whereas this will handle displaying the logs to the terminal & a log file
        .chain({
            let mut logger = fern::Dispatch::new()
                .format(|out, message, record| {
                    out.finish(format_args!(
                        "[{}] {}: {}",
                        record.level(),
                        {
                            let target = record.target();
                            target.find(':').map_or(target, |pos| &target[..pos])
                        },
                        message
                    ))
                })
                .chain(std::io::stderr());

            match fern::log_file("/tmp/installer.log") {
                Ok(log) => logger = logger.chain(log),
                Err(why) => {
                    eprintln!("failed to create log file at /tmp/installer.log: {}", why);
                }
            };

            // If the home directory exists, add a log there as well.
            // If the Desktop directory exists within the home directory, write the logs there.
            if let Some(home) = env::home_dir() {
                let desktop = home.join("Desktop");
                let log = if desktop.is_dir() {
                    fern::log_file(&desktop.join("installer.log"))
                } else {
                    fern::log_file(&home.join("installer.log"))
                };

                match log {
                    Ok(log) => logger = logger.chain(log),
                    Err(why) => {
                        eprintln!("failed to set up logging for the home directory: {}", why);
                    }
                }
            }

            logger
        }).apply()?;

    Ok(())
}

pub fn deactivate_logical_devices() -> io::Result<()> {
    for luks_pv in encrypted_devices()? {
        info!("deactivating encrypted device named {}", luks_pv);
        if let Some(vg) = pvs()?.get(&PathBuf::from(["/dev/mapper/", &luks_pv].concat())) {
            match *vg {
                Some(ref vg) => {
                    vgdeactivate(vg).and_then(|_| cryptsetup_close(CloseBy::Name(&luks_pv)))?;
                },
                None => {
                    cryptsetup_close(CloseBy::Name(&luks_pv))?;
                },
            }
        }
    }

    Ok(())
}

/// Checks if the given name already exists as a device in the device map list.
pub fn device_map_exists(name: &str) -> bool {
    dmlist().ok().map_or(false, |list| list.contains(&name.into()))
}

/// Gets the minimum number of sectors required. The input should be in sectors, not bytes.
///
/// The number of sectors required is calculated through:
///
/// - The value in `/cdrom/casper/filesystem.size`
/// - The size of a default boot / esp partition
/// - The size of a default swap partition
/// - The size of a default recovery partition.
///
/// The input parameter will undergo a max comparison to the estimated minimum requirement.
pub fn minimum_disk_size(default: u64) -> u64 {
    let casper_size = File::open("/cdrom/casper/filesystem.size")
        .ok()
        .and_then(|mut file| {
            let capacity = file.metadata().ok().map_or(0, |m| m.len());
            let mut buffer = String::with_capacity(capacity as usize);
            file.read_to_string(&mut buffer)
                .ok()
                .and_then(|_| buffer[..buffer.len() - 1].parse::<u64>().ok())
        })
        // Convert the number of bytes read into sectors required + 1
        .map(|bytes| (bytes / 512) + 1)
        .map_or(default, |size| size.max(default));

    casper_size + DEFAULT_ESP_SECTORS + DEFAULT_RECOVER_SECTORS + DEFAULT_SWAP_SECTORS
}

/// Installation step
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Step {
    Init,
    Partition,
    Extract,
    Configure,
    Bootloader,
}

impl From<ReinstallError> for io::Error {
    fn from(why: ReinstallError) -> io::Error {
        io::Error::new(
            io::ErrorKind::Other,
            format!("{}", why)
        )
    }
}

pub const MODIFY_BOOT_ORDER: u8 = 0b01;
pub const INSTALL_HARDWARE_SUPPORT: u8 = 0b10;

/// Installer configuration
#[derive(Debug)]
pub struct Config {
    /// Hostname to assign to the installed system.
    pub hostname: String,
    /// The keyboard layout to use with the installed system (such as "us").
    pub keyboard_layout: String,
    /// An optional keyboard model (such as "pc105") to define the keyboard's model.
    pub keyboard_model: Option<String>,
    /// An optional variant of the keyboard (such as "dvorak").
    pub keyboard_variant: Option<String>,
    /// The UUID of the old root partition, for retaining user accounts.
    pub old_root: Option<String>,
    /// The locale to use for the installed system.
    pub lang: String,
    /// The file that contains a list of packages to remove.
    pub remove: String,
    /// The archive (`tar` or `squashfs`) which contains the base system.
    pub squashfs: String,
    /// Some flags to control the behavior of the installation.
    pub flags: u8,
}

/// Installer error
#[derive(Debug)]
pub struct Error {
    pub step: Step,
    pub err:  io::Error,
}

/// Installer status
#[derive(Copy, Clone, Debug)]
pub struct Status {
    pub step:    Step,
    pub percent: i32,
}

/// An installer object
pub struct Installer {
    error_cb:  Option<Box<FnMut(&Error)>>,
    status_cb: Option<Box<FnMut(&Status)>>,
}

impl Installer {
    /// Create a new installer object
    ///
    /// ```ignore,rust
    /// use distinst::Installer;
    /// let installer = Installer::new();
    /// ```
    pub fn default() -> Installer {
        Installer {
            error_cb:  None,
            status_cb: None,
        }
    }

    /// Send an error message
    ///
    /// ```ignore,rust
    /// use distinst::{Error, Installer, Step};
    /// use std::io;
    /// let mut installer = Installer::new();
    /// installer.emit_error(&Error {
    ///     step: Step::Extract,
    ///     err:  io::Error::new(io::ErrorKind::NotFound, "File not found"),
    /// });
    /// ```
    pub fn emit_error(&mut self, error: &Error) {
        if let Some(ref mut cb) = self.error_cb {
            cb(error);
        }
    }

    /// Set the error callback
    ///
    /// ```ignore,rust
    /// use distinst::Installer;
    /// let mut installer = Installer::new();
    /// installer.on_error(|error| println!("{:?}", error));
    /// ```
    pub fn on_error<F: FnMut(&Error) + 'static>(&mut self, callback: F) {
        self.error_cb = Some(Box::new(callback));
    }

    /// Send a status message
    ///
    /// ```ignore,rust
    /// use distinst::{Installer, Status, Step};
    /// let mut installer = Installer::new();
    /// installer.emit_status(&Status {
    ///     step:    Step::Extract,
    ///     percent: 50,
    /// });
    /// ```
    pub fn emit_status(&mut self, status: Status) {
        if let Some(ref mut cb) = self.status_cb {
            cb(&status);
        }
    }

    /// Set the status callback
    ///
    /// ```ignore,rust
    /// use distinst::Installer;
    /// let mut installer = Installer::new();
    /// installer.on_status(|status| println!("{:?}", status));
    /// ```
    pub fn on_status<F: FnMut(&Status) + 'static>(&mut self, callback: F) {
        self.status_cb = Some(Box::new(callback));
    }

    fn initialize<F: FnMut(i32)>(
        disks: &mut Disks,
        config: &Config,
        mut callback: F,
    ) -> io::Result<(PathBuf, Vec<String>)> {
        info!("Initializing");

        let fetch_squashfs = || match Path::new(&config.squashfs).canonicalize() {
            Ok(squashfs) => if squashfs.exists() {
                info!("config.squashfs: found at {}", squashfs.display());
                Ok(squashfs)
            } else {
                error!("config.squashfs: supplied file does not exist");
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "invalid squashfs path",
                ))
            },
            Err(err) => {
                error!("config.squashfs: {}", err);
                Err(err)
            }
        };

        let fetch_packages = || {
            let mut remove_pkgs = Vec::new();
            {
                let file = match File::open(&config.remove) {
                    Ok(file) => file,
                    Err(err) => {
                        error!("config.remove: {}", err);
                        return Err(err);
                    }
                };

                // Takes a locale, such as `en_US.UTF-8`, and changes it into `en_US`.
                let locale = match config.lang.find('.') {
                    Some(pos) => &config.lang[..pos],
                    None => &config.lang
                };

                // A list of language packages in the ISO that should be kept, even if they are
                // listed in the remove manifest.
                let lang_output = Command::new("check-language-support")
                    .args(&[&["--language=", locale].concat(), "--show-installed"])
                    .output()
                    .map_err(|why| io::Error::new(
                        io::ErrorKind::Other,
                        format!("failed to spawn check-language-support: {}", why)
                    ))?;

                // Packages in the output are delimited with spaces.
                let lang_packs = lang_output.stdout.split(|&x| x == b' ')
                        .map(|x| String::from_utf8_lossy(x))
                        .collect::<Vec<_>>();

                // Collects the packages that are to be removed from the install.
                for line_res in io::BufReader::new(file).lines() {
                    match line_res {
                        // Only add package if it is not contained within lang_packs.
                        Ok(line) => if !lang_packs.iter().any(|x| x == &line) {
                            remove_pkgs.push(line)
                        },
                        Err(err) => {
                            error!("config.remove: {}", err);
                            return Err(err);
                        }
                    }
                }
            }

            Ok(remove_pkgs)
        };

        let ((res_a, res_b), (res_c, res_d)): (
            (io::Result<()>, io::Result<Vec<String>>),
            (io::Result<()>, io::Result<PathBuf>)
        ) = rayon::join(
            || rayon::join(
                || {
                    // Deactivate any open logical volumes & close any encrypted partitions.
                    if let Err(why) = disks.deactivate_device_maps() {
                        error!("lvm deactivation error: {}", why);
                        return Err(io::Error::new(io::ErrorKind::Other, format!("{}", why)));
                    }

                    // Unmount any mounted devices.
                    if let Err(why) = disks.unmount_devices() {
                        error!("device unmount error: {}", why);
                        return Err(io::Error::new(io::ErrorKind::Other, format!("{}", why)));
                    }

                    Ok(())
                },
                fetch_packages
            ),
            || rayon::join(
                || {
                    disks.verify_keyfile_paths()?;
                    Ok(())
                },
                fetch_squashfs
            )
        );

        let (remove_pkgs, squashfs) = res_a
            .and(res_c)
            .and(res_b)
            .and_then(|pkgs| res_d.map(|squashfs| (pkgs, squashfs)))?;

        let disks_ptr = &*disks as *const Disks;
        {
            let borrowed: &Disks = unsafe { &*disks_ptr };
            disks.physical.iter_mut().map(|disk| {
                // This will help us when we are testing in a dev environment.
                if disk.contains_mount("/", borrowed) {
                    return Ok(());
                }

                if let Err(why) = disk.unmount_all_partitions_with_target() {
                    error!("unable to unmount partitions");
                    return Err(io::Error::new(io::ErrorKind::Other, format!("{}", why)));
                }

                Ok(())
            }).collect::<io::Result<()>>()?;
        }

        callback(100);

        Ok((squashfs, remove_pkgs))
    }

    /// Apply all partitioning and formatting changes to the disks
    /// configuration specified.
    fn partition<F: FnMut(i32)>(disks: &mut Disks, mut callback: F) -> io::Result<()> {
        let (pvs_result, commit_result): (
            io::Result<BTreeMap<PathBuf, Option<String>>>,
            io::Result<()>
        ) = rayon::join(
            || {
                // This collection of physical volumes and their optional volume groups
                // will be used to obtain a list of volume groups associated with our
                // modified partitions.
                pvs().map_err(|why| io::Error::new(io::ErrorKind::Other, format!("{}", why)))
            },
            || {
                // Perform layout changes serially, due to libparted thread safety issues,
                // and collect a list of partitions to format which can be done in parallel.
                // Once partitions have been formatted in parallel, reload the disk configuration.
                let mut partitions_to_format = FormatPartitions(Vec::new());
                for disk in disks.get_physical_devices_mut() {
                    info!("{}: Committing changes to disk", disk.path().display());
                    if let Some(partitions) = disk.commit()
                        .map_err(|why| io::Error::new(io::ErrorKind::Other, format!("{}", why)))?
                    {
                        partitions_to_format.0.extend_from_slice(&partitions.0);
                    }
                }

                partitions_to_format.format()?;

                // Optimization: possibly do this while formatting partitions?
                for disk in &mut disks.physical {
                    disk.reload()?;
                }

                Ok(())
            }
        );

        let pvs = commit_result.and(pvs_result)?;

        callback(100);

        // Utilizes the physical volume collection to generate a vector of volume
        // groups which we will need to deactivate pre-`blockdev`, and will be
        // reactivated post-`blockdev`.
        let vgs = disks
            .get_physical_devices()
            .iter()
            .flat_map(|disk| {
                disk.get_partitions()
                    .iter()
                    .filter_map(|part| match pvs.get(&part.device_path) {
                        Some(&Some(ref vg)) => Some(vg.clone()),
                        _ => None,
                    })
            })
            .unique()
            .collect::<Vec<String>>();

        // Deactivate logical volumes so that blockdev will not fail.
        vgs.iter()
            .map(|vg| vgdeactivate(vg))
            .collect::<io::Result<()>>()?;

        // Ensure that the logical volumes have had time to deactivate.
        sleep(Duration::from_secs(1));

        // This is to ensure that everything's been written and the OS is ready to
        // proceed.
        disks.physical.par_iter().for_each(|disk| {
            let _ = blockdev(&disk.path(), &["--flushbufs", "--rereadpt"]);
        });

        // Give a bit of time to ensure that logical volumes can be re-activated.
        sleep(Duration::from_secs(1));

        // Reactivate the logical volumes.
        vgs.iter()
            .map(|vg| vgactivate(vg))
            .collect::<io::Result<()>>()?;

        disks
            .commit_logical_partitions()
            .map_err(|why| io::Error::new(io::ErrorKind::Other, format!("{}", why)))
    }

    /// Mount all target paths defined within the provided `disks`
    /// configuration.
    fn mount(disks: &Disks, chroot: &Path) -> io::Result<Mounts> {
        let physical_targets = disks
            .get_physical_devices()
            .iter()
            .flat_map(|disk| disk.file_system.as_ref().into_iter().chain(disk.partitions.iter()))
            .filter(|part| part.target.is_some() && part.filesystem.is_some());

        let logical_targets = disks
            .get_logical_devices()
            .iter()
            .flat_map(|disk| disk.file_system.as_ref().into_iter().chain(disk.partitions.iter()))
            .filter(|part| part.target.is_some() && part.filesystem.is_some());

        let targets = physical_targets.chain(logical_targets);

        let mut mounts = Vec::new();

        // The mount path will actually consist of the target concatenated with the
        // root. NOTE: It is assumed that the target is an absolute path.
        let paths: BTreeMap<PathBuf, (PathBuf, &'static str)> = targets
            .map(|target| {
                // Path mangling commences here, since we need to concatenate an absolute
                // path onto another absolute path, and the standard library opts for
                // overwriting the original path when doing that.
                let target_mount: PathBuf = {
                    // Ensure that the chroot path has the ending '/'.
                    let chroot = chroot.as_os_str().as_bytes();
                    let mut target_mount: Vec<u8> = if chroot[chroot.len() - 1] == b'/' {
                        chroot.to_owned()
                    } else {
                        let mut temp = chroot.to_owned();
                        temp.push(b'/');
                        temp
                    };

                    // Cut the ending '/' from the target path if it exists.
                    let target_path = target.target.as_ref().unwrap().as_os_str().as_bytes();
                    let target_path = if target_path[0] == b'/' {
                        if target_path.len() > 1 {
                            &target_path[1..]
                        } else {
                            b""
                        }
                    } else {
                        target_path
                    };

                    // Append the target path to the chroot, and return it as a path type.
                    target_mount.extend_from_slice(target_path);
                    PathBuf::from(OsString::from_vec(target_mount))
                };

                let fs = match target.filesystem.unwrap() {
                    FileSystemType::Fat16 | FileSystemType::Fat32 => "vfat",
                    fs => fs.into(),
                };

                (target_mount, (target.device_path.clone(), fs))
            })
            .collect();

        // Each mount directory will be created and then mounted before progressing to
        // the next mount in the map. The BTreeMap that the mount targets were
        // collected into will ensure that mounts are created and mounted in
        // the correct order.
        for (target_mount, (device_path, filesystem)) in paths {
            if let Err(why) = fs::create_dir_all(&target_mount) {
                error!("unable to create '{}': {}", why, target_mount.display());
            }

            info!(
                "mounting {} to {}, with {}",
                device_path.display(),
                target_mount.display(),
                filesystem
            );

            mounts.push(Mount::new(
                &device_path,
                &target_mount,
                filesystem,
                0,
                None,
            )?);
        }

        Ok(Mounts(mounts))
    }

    /// Extracts the squashfs image into the new install
    fn extract<P: AsRef<Path>, F: FnMut(i32)>(
        squashfs: P,
        mount_dir: P,
        callback: F,
    ) -> io::Result<()> {
        info!("Extracting {}", squashfs.as_ref().display());
        squashfs::extract(squashfs, mount_dir, callback)?;

        Ok(())
    }

    /// Configures the new install after it has been extracted.
    fn configure<P: AsRef<Path>, S: AsRef<str>, I: IntoIterator<Item = S>, F: FnMut(i32)>(
        disks: &Disks,
        mount_dir: P,
        bootloader: Bootloader,
        config: &Config,
        remove_pkgs: I,
        mut callback: F,
    ) -> io::Result<()> {
        let mount_dir = mount_dir.as_ref().canonicalize().unwrap();
        info!("Configuring on {}", mount_dir.display());
        let configure_dir = TempDir::new_in(mount_dir.join("tmp"), "distinst")?;
        let configure = configure_dir.path().join("configure.sh");

        let install_pkgs: &mut Vec<&str> = &mut match bootloader {
            Bootloader::Bios => vec!["grub-pc"],
            // We use kernelstub for EFI instead of GRUB, for Pop!_OS
            Bootloader::Efi if OS_RELEASE.name == "Pop!_OS"=> vec!["kernelstub"],
            // Ubuntu does not provide kernelstub, so it must use grub-efi instead.
            Bootloader::Efi => vec!["grub-efi"],
        };

        let configure_script = || {
            // Write the installer's intallation script to the chroot's temporary directory.
            info!("writing /tmp/configure.sh");
            file_create!(&configure, [include_bytes!("scripts/configure.sh")]);
            Ok(())
        };

        let lvm_autodetection = || {
            // Ubuntu's LVM auto-detection doesn't seem to work for activating root volumes.
            info!("applying LVM initramfs autodetect workaround");
            fs::create_dir_all(mount_dir.join("etc/initramfs-tools/scripts/local-top/"))?;
            let lvm_fix = mount_dir.join("etc/initramfs-tools/scripts/local-top/lvm-workaround");
            file_create!(
                lvm_fix,
                0o1755,
                [include_bytes!("scripts/lvm-workaround.sh")]
            );
            Ok(())
        };

        let generate_fstabs = || {
            let (crypttab, fstab) = disks.generate_fstabs();

            let (a, b) = rayon::join(
                || {
                    info!("writing /etc/crypttab");
                    file_create!(mount_dir.join("etc/crypttab"), [crypttab.as_bytes()]);
                    Ok(())
                },
                || {
                    info!("writing /etc/fstab");
                    file_create!(
                        mount_dir.join("etc/fstab"),
                        [FSTAB_HEADER, fstab.as_bytes()]
                    );
                    Ok(())
                }
            );

            a.and(b)
        };

        let disable_nvidia = {
            let ((a, disable_nvidia), (b, c)): (
                (io::Result<()>, io::Result<bool>),
                (io::Result<()>, io::Result<()>)
            ) = rayon::join(
                || rayon::join(
                    configure_script,
                    || {
                        if config.flags & INSTALL_HARDWARE_SUPPORT != 0 {
                            hardware_support::append_packages(install_pkgs);
                        }

                        hardware_support::blacklist::disable_external_graphics(&mount_dir)
                    }
                ),
                || rayon::join(lvm_autodetection, generate_fstabs)
            );

            a.and(b).and(c).and(disable_nvidia)?
        };

        callback(60);

        {
            info!(
                "chrooting into target on {}",
                mount_dir.display()
            );
            let mut chroot = Chroot::new(&mount_dir)?;
            let efivars_mount = mount_efivars(&mount_dir)?;
            let cdrom_mount = mount_cdrom(&mount_dir)?;
            let cdrom_target = cdrom_mount.as_ref().map(|x| x.dest().to_path_buf());

            let configure_chroot = configure.strip_prefix(&mount_dir).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Path::strip_prefix failed: {}", err),
                )
            })?;

            callback(75);

            use std::iter;

            let root_entry = {
                info!("retrieving root partition");
                disks
                    .get_physical_devices()
                    .iter()
                    .flat_map(|disk| {
                        let iterator: Box<Iterator<Item = &PartitionInfo>> = if let Some(ref fs) = disk.file_system {
                            Box::new(iter::once(fs).chain(disk.partitions.iter()))
                        } else {
                            Box::new(disk.partitions.iter())
                        };
                        iterator
                    })
                    .filter_map(|part| part.get_block_info())
                    .find(|entry| entry.mount() == "/")
                    .or_else(|| disks
                        .get_logical_devices()
                        .iter()
                        .flat_map(|disk| {
                            let iterator: Box<Iterator<Item = &PartitionInfo>> = if let Some(ref fs) = disk.file_system {
                                Box::new(iter::once(fs).chain(disk.partitions.iter()))
                            } else {
                                Box::new(disk.partitions.iter())
                            };
                            iterator
                        })
                        .filter_map(|part| part.get_block_info())
                        .find(|entry| entry.mount() == "/"))
                    .ok_or_else(|| io::Error::new(
                        io::ErrorKind::Other,
                        "root partition not found",
                    ))?
            };

            let luks_uuid = misc::from_uuid(&root_entry.uuid)
                .and_then(|ref path| misc::resolve_to_physical(path.file_name().unwrap().to_str().unwrap()))
                .and_then(|ref path| misc::get_uuid(path))
                .and_then(|uuid| if uuid == root_entry.uuid { None } else { Some(uuid)});

            let root_uuid = &root_entry.uuid;
            update_recovery_config(&mount_dir, &root_uuid, luks_uuid.as_ref().map(|x| x.as_str()))?;

            info!(
                "will install {:?} bootloader packages",
                install_pkgs
            );

            let args = {
                let mut args = Vec::new();

                // Clear existing environment
                args.push("-i".to_string());

                // Set hostname to be set
                args.push(format!("HOSTNAME={}", config.hostname));

                // Set language to config setting
                args.push(format!("LANG={}", config.lang));

                // Set preferred keyboard layout
                args.push(format!("KBD_LAYOUT={}", config.keyboard_layout));

                if disable_nvidia {
                    args.push("DISABLE_NVIDIA=1".into());
                }

                if let Some(ref model) = config.keyboard_model {
                    args.push(format!("KBD_MODEL={}", model));
                }

                if let Some(ref variant) = config.keyboard_variant {
                    args.push(format!("KBD_VARIANT={}", variant));
                }

                // Set root UUID
                args.push(format!("ROOT_UUID={}", root_uuid));

                args.push(format!("LUKS_UUID={}", match luks_uuid.as_ref() {
                    Some(ref uuid) => uuid.as_str(),
                    None => ""
                }));

                // Run configure script with bash
                args.push("bash".to_string());

                // Path to configure script in chroot
                args.push(configure_chroot.to_str().unwrap().to_string());

                for pkg in install_pkgs {
                    // Install bootloader packages
                    args.push(pkg.to_string());
                }

                for pkg in remove_pkgs {
                    // Remove installer packages
                    args.push(format!("-{}", pkg.as_ref()));
                }

                args
            };

            let status = chroot.command("/usr/bin/env", args.iter())?;

            if !status.success() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("configure.sh failed with status: {}", status),
                ));
            }

            // Ensure that the cdrom binding is unmounted before the chroot.
            drop(cdrom_mount);
            drop(efivars_mount);

            cdrom_target.map(|target| fs::remove_dir(&target));
            chroot.unmount(false)?;
        }

        configure_dir.close()?;

        callback(100);

        Ok(())
    }

    /// Installs and configures the boot loader after it has been configured.
    fn bootloader<F: FnMut(i32)>(
        disks: &Disks,
        mount_dir: &Path,
        bootloader: Bootloader,
        config: &Config,
        mut callback: F,
    ) -> io::Result<()> {
        // Obtain the root device & partition, with an optional EFI device & partition.
        let ((root_dev, _root_part), boot_opt) = disks.get_base_partitions(bootloader);

        let mut efi_part_num = 0;
        let bootloader_dev = match boot_opt {
            Some((dev, dev_part)) => {
                efi_part_num = dev_part.number;
                dev
            }
            None => root_dev,
        };

        info!(
            "{}: installing bootloader for {:?}",
            bootloader_dev.display(),
            bootloader
        );

        {
            let efi_path = {
                let chroot = mount_dir.as_os_str().as_bytes();
                let mut target_mount: Vec<u8> = if chroot[chroot.len() - 1] == b'/' {
                    chroot.to_owned()
                } else {
                    let mut temp = chroot.to_owned();
                    temp.push(b'/');
                    temp
                };

                target_mount.extend_from_slice(b"boot/efi/");
                PathBuf::from(OsString::from_vec(target_mount))
            };

            // Also ensure that the /boot/efi directory is created.
            if bootloader == Bootloader::Efi && boot_opt.is_some() {
                fs::create_dir_all(&efi_path)?;
            }

            {
                let mut chroot = Chroot::new(mount_dir)?;
                let efivars_mount = mount_efivars(&mount_dir)?;

                match bootloader {
                    Bootloader::Bios => {
                        let status = chroot.command(
                            "grub-install",
                            &[
                                // Recreate device map
                                "--recheck".into(),
                                // Install for BIOS
                                "--target=i386-pc".into(),
                                // Install to the bootloader_dev device
                                bootloader_dev.to_str().unwrap().to_owned(),
                            ],
                        )?;

                        if !status.success() {
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                format!("grub-install failed with status: {}", status),
                            ));
                        }
                    }
                    Bootloader::Efi => {
                        let status = chroot.command(
                            "bootctl",
                            &[
                                // Install systemd-boot
                                "install",
                                // Provide path to ESP
                                "--path=/boot/efi",
                                // Do not set EFI variables
                                "--no-variables",
                            ][..],
                        )?;

                        if !status.success() {
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                format!("bootctl failed with status: {}", status),
                            ));
                        }

                        if config.flags & MODIFY_BOOT_ORDER != 0 {
                            let efi_part_num = efi_part_num.to_string();
                            let args: &[&OsStr] = &[
                                "--create".as_ref(),
                                "--disk".as_ref(),
                                bootloader_dev.as_ref(),
                                "--part".as_ref(),
                                efi_part_num.as_ref(),
                                "--write-signature".as_ref(),
                                "--label".as_ref(),
                                os_release::OS_RELEASE.pretty_name.as_ref(),
                                "--loader".as_ref(),
                                "\\EFI\\systemd\\systemd-bootx64.efi".as_ref(),
                            ][..];

                            let status = chroot.command("efibootmgr", args)?;

                            if !status.success() {
                                return Err(io::Error::new(
                                    io::ErrorKind::Other,
                                    format!("efibootmgr failed with status: {}", status),
                                ));
                            }
                        }
                    }
                }

                drop(efivars_mount);
                chroot.unmount(false)?;
            }
        }

        callback(100);

        Ok(())
    }

    /// The user will use this method to hand off installation tasks to distinst.
    ///
    /// The `disks` field contains all of the disks configuration information that will be
    /// applied before installation. The `config` field provides configuration details that
    /// will be applied when configuring the new installation.
    ///
    /// If `config.old_root` is set, then home at that location will be retained.
    pub fn install(&mut self, mut disks: Disks, config: &Config) -> io::Result<()> {
        let account_files;

        let backup = if let Some(ref old_root_uuid) = config.old_root {
            info!("installing while retaining home");

            let current_disks =
                Disks::probe_devices().map_err(|why| ReinstallError::DiskProbe { why })?;
            let old_root = current_disks
                .get_partition_by_uuid(old_root_uuid)
                .ok_or(ReinstallError::NoRootPartition)?;
            let new_root = disks
                .get_partition_with_target(Path::new("/"))
                .ok_or(ReinstallError::NoRootPartition)?;

            let (home, home_is_root) = disks
                .get_partition_with_target(Path::new("/home"))
                .map_or((old_root, true), |p| (p, false));

            if home.will_format() {
                return Err(ReinstallError::ReformattingHome.into());
            }

            let home_path = home.get_device_path();
            let root_path = new_root.get_device_path().to_path_buf();
            let root_fs = new_root
                .filesystem
                .ok_or_else(|| ReinstallError::NoFilesystem)?;
            let old_root_fs = old_root
                .filesystem
                .ok_or_else(|| ReinstallError::NoFilesystem)?;
            let home_fs = home.filesystem.ok_or_else(|| ReinstallError::NoFilesystem)?;

            account_files = AccountFiles::new(old_root.get_device_path(), old_root_fs)?;
            let backup = Backup::new(home_path, home_fs, home_is_root, &account_files)?;

            validate_before_removing(&disks, &config.squashfs, home_path, home_fs)?;

            Some((backup, root_path, root_fs))
        } else {
            None
        };

        if !hostname::is_valid(&config.hostname) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hostname is not valid",
            ));
        }

        let bootloader = Bootloader::detect();
        disks.verify_partitions(bootloader)?;

        let mut status = Status {
            step:    Step::Init,
            percent: 0,
        };

        macro_rules! apply_step {
            // When a step is provided, that step will be set. This branch will then invoke a
            // second call to the macro, which will execute the branch below this one.
            ($step:expr, $msg:expr, $action:expr) => {{
                unsafe {
                    libc::sync();
                }

                if KILL_SWITCH.load(Ordering::SeqCst) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "process killed"));
                }

                status.step = $step;
                status.percent = 0;
                self.emit_status(status);

                apply_step!($msg, $action);
            }};
            ($msg:expr, $action:expr) => {{
                info!("starting {} step", $msg);
                match $action {
                    Ok(value) => value,
                    Err(err) => {
                        error!("{} error: {}", $msg, err);
                        let error = Error {
                            step: status.step,
                            err,
                        };
                        self.emit_error(&error);
                        return Err(error.err);
                    }
                }
            }};
        }

        macro_rules! percent {
            () => {
                |percent| {
                    status.percent = percent;
                    self.emit_status(status);
                }
            }
        }

        info!("installing {:?} with {:?}", config, bootloader);
        self.emit_status(status);

        let (squashfs, remove_pkgs) = apply_step!("initializing", {
            Installer::initialize(&mut disks, config, percent!())
        });

        apply_step!(Step::Partition, "partitioning", {
            Installer::partition(&mut disks, percent!())
        });

        // Mount the temporary directory, and all of our mount targets.
        const CHROOT_ROOT: &str = "distinst";
        info!(
            "mounting temporary chroot directory at {}",
            CHROOT_ROOT
        );

        {
            let mount_dir = TempDir::new(CHROOT_ROOT)?;

            {
                let mut mounts = Installer::mount(&disks, mount_dir.path())?;

                if PARTITIONING_TEST.load(Ordering::SeqCst) {
                    info!("PARTITION_TEST enabled: exiting before unsquashing");
                    return Ok(());
                }

                apply_step!(Step::Extract, "extraction", {
                    Installer::extract(squashfs.as_path(), mount_dir.path(), percent!())
                });

                apply_step!(Step::Configure, "configuration", {
                    Installer::configure(
                        &disks,
                        mount_dir.path(),
                        bootloader,
                        &config,
                        &remove_pkgs,
                        percent!(),
                    )
                });

                apply_step!(Step::Bootloader, "bootloader", {
                    Installer::bootloader(&disks, mount_dir.path(), bootloader, &config, percent!())
                });

                mounts.unmount(false)?;
            }

            mount_dir.close()?;
        }

        if let Some((backup, root_path, root_fs)) = backup {
            backup.restore(&root_path, root_fs)?;
        }

        let _ = deactivate_logical_devices();

        Ok(())
    }

    /// Get a list of disks, skipping loopback devices
    ///
    /// ```ignore,rust
    /// use distinst::Installer;
    /// let installer = Installer::new();
    /// let disks = installer.disks().unwrap();
    /// ```
    pub fn disks(&self) -> io::Result<Disks> {
        info!("probing disks on system");
        Disks::probe_devices()
            .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{}", err)))
    }
}

fn update_recovery_config(mount: &Path, root_uuid: &str, luks_uuid: Option<&str>) -> io::Result<()> {
    fn remove_boot(mount: &Path, uuid: &str) -> io::Result<()> {
        for directory in mount.join("boot/efi/EFI").read_dir()? {
            let entry = directory?;
            let full_path = entry.path();
            if let Some(path) = entry.file_name().to_str() {
                if path.ends_with(uuid) {
                    info!("removing old boot files for {}", path);
                    fs::remove_dir_all(&full_path)?;
                }
            }
        }

        Ok(())
    }

    let recovery_path = Path::new("/cdrom/recovery.conf");
    if recovery_path.exists() {
        let recovery_conf = &mut EnvFile::new(recovery_path)?;

        let luks_value = if let Some(uuid) = luks_uuid {
            if root_uuid == uuid { "" } else { uuid }
        } else {
            ""
        };

        recovery_conf.update("LUKS_UUID", luks_value);

        remount_rw("/cdrom")
            .and_then(|_| {
                recovery_conf.update("OEM_MODE", "0");
                recovery_conf.get("ROOT_UUID").ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "no ROOT_UUID found in /cdrom/recovery.conf",
                    )
                })
            })
            .and_then(|old_uuid| remove_boot(mount, old_uuid))
            .and_then(|_| {
                recovery_conf.update("ROOT_UUID", root_uuid);
                recovery_conf.write()
            })?;
    }

    Ok(())
}

fn mount_cdrom(mount_dir: &Path) -> io::Result<Option<Mount>> {
    let cdrom_source = Path::new("/cdrom");
    let cdrom_target = mount_dir.join("cdrom");
    mount_bind_if_exists(&cdrom_source, &cdrom_target)
}

fn mount_efivars(mount_dir: &Path) -> io::Result<Option<Mount>> {
    let efivars_source = Path::new("/sys/firmware/efi/efivars");
    let efivars_target = mount_dir.join("sys/firmware/efi/efivars");
    mount_bind_if_exists(&efivars_source, &efivars_target)
}

fn mount_bind_if_exists(source: &Path, target: &Path) -> io::Result<Option<Mount>> {
    use disk::mount::BIND;
    if source.exists() {
        let _ = fs::create_dir_all(&target);
        Ok(Some(Mount::new(&source, &target, "none", BIND, None)?))
    } else {
        Ok(None)
    }
}
