use nix;
use nix::mount::{mount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{self, fork, ForkResult};
use std::env;
use std::fs;
use std::io;
use std::io::prelude::*;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process;
use std::string::String;
use xdg;

mod mkdtemp;

const NONE: Option<&'static [u8]> = None;

fn bind_mount(source: &Path, dest: &Path) {
    if let Err(e) = mount(
        Some(source),
        dest,
        Some("none"),
        MsFlags::MS_BIND | MsFlags::MS_REC,
        NONE,
    ) {
        eprintln!(
            "failed to bind mount {} to {}: {}",
            source.display(),
            dest.display(),
            e
        );
    }
}

pub struct RunChroot<'a> {
    rootdir: &'a Path,
}

impl<'a> RunChroot<'a> {
    fn new(rootdir: &'a Path) -> Self {
        Self { rootdir }
    }

    fn bind_mount_directory(&self, entry: &fs::DirEntry) {
        let mountpoint = self.rootdir.join(entry.file_name());
        if let Err(e) = fs::create_dir(&mountpoint) {
            if e.kind() != io::ErrorKind::AlreadyExists {
                panic!("failed to create {}: {}", &mountpoint.display(), e);
            }
        }

        bind_mount(&entry.path(), &mountpoint)
    }

    fn bind_mount_file(&self, entry: &fs::DirEntry) {
        let mountpoint = self.rootdir.join(entry.file_name());
        fs::File::create(&mountpoint)
            .unwrap_or_else(|err| panic!("failed to create {}: {}", &mountpoint.display(), err));

        bind_mount(&entry.path(), &mountpoint)
    }

    fn mirror_symlink(&self, entry: &fs::DirEntry) {
        let path = entry.path();
        let target = fs::read_link(&path)
            .unwrap_or_else(|err| panic!("failed to resolve symlink {}: {}", &path.display(), err));
        let link_path = self.rootdir.join(entry.file_name());
        symlink(&target, &link_path).unwrap_or_else(|_| {
            panic!(
                "failed to create symlink {} -> {}",
                &link_path.display(),
                &target.display()
            )
        });
    }

    fn bind_mount_direntry(&self, entry: io::Result<fs::DirEntry>) {
        let entry = entry.expect("error while listing from /nix directory");
        // do not bind mount an existing nix installation
        if entry.file_name() == PathBuf::from("nix") {
            return;
        }
        let path = entry.path();
        let stat = entry
            .metadata()
            .unwrap_or_else(|err| panic!("cannot get stat of {}: {}", path.display(), err));
        if stat.is_dir() {
            self.bind_mount_directory(&entry);
        } else if stat.is_file() {
            self.bind_mount_file(&entry);
        } else if stat.file_type().is_symlink() {
            self.mirror_symlink(&entry);
        }
    }

    fn run_chroot(&self, nixdir: &Path, cmd: &str, args: &[String]) {
        let cwd = env::current_dir().expect("cannot get current working directory");

        let uid = unistd::getuid();
        let gid = unistd::getgid();

        unshare(CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUSER).expect("unshare failed");

        // bind mount all / stuff into rootdir
        let nix_root = PathBuf::from("/");
        let dir = fs::read_dir(&nix_root).expect("failed to list /nix directory");
        for entry in dir {
            self.bind_mount_direntry(entry);
        }

        // mount the store
        let nix_mount = self.rootdir.join("nix");
        fs::create_dir(&nix_mount)
            .unwrap_or_else(|err| panic!("failed to create {}: {}", &nix_mount.display(), err));
        mount(
            Some(nixdir),
            &nix_mount,
            Some("none"),
            MsFlags::MS_BIND | MsFlags::MS_REC,
            NONE,
        )
        .unwrap_or_else(|err| panic!("failed to bind mount {} to /nix: {}", nixdir.display(), err));

        // chroot
        unistd::chroot(self.rootdir)
            .unwrap_or_else(|err| panic!("chroot({}): {}", self.rootdir.display(), err));

        env::set_current_dir("/").expect("cannot change directory to /");

        // fixes issue #1 where writing to /proc/self/gid_map fails
        // see user_namespaces(7) for more documentation
        if let Ok(mut file) = fs::File::create("/proc/self/setgroups") {
            let _ = file.write_all(b"deny");
        }

        let mut uid_map =
            fs::File::create("/proc/self/uid_map").expect("failed to open /proc/self/uid_map");
        uid_map
            .write_all(format!("{} {} 1", uid, uid).as_bytes())
            .expect("failed to write new uid mapping to /proc/self/uid_map");

        let mut gid_map =
            fs::File::create("/proc/self/gid_map").expect("failed to open /proc/self/gid_map");
        gid_map
            .write_all(format!("{} {} 1", gid, gid).as_bytes())
            .expect("failed to write new gid mapping to /proc/self/gid_map");

        // restore cwd
        env::set_current_dir(&cwd)
            .unwrap_or_else(|_| panic!("cannot restore working directory {}", cwd.display()));

        let err = process::Command::new(cmd)
            .args(args)
            .env("NIX_CONF_DIR", "/nix/etc/nix")
            .exec();

        eprintln!("failed to execute {}: {}", &cmd, err);
        process::exit(1);
    }
}

fn wait_for_child(rootdir: &Path, child_pid: unistd::Pid) -> ! {
    let mut exit_status = 1;
    loop {
        match waitpid(child_pid, Some(WaitPidFlag::WUNTRACED)) {
            Ok(WaitStatus::Signaled(child, Signal::SIGSTOP, _)) => {
                let _ = kill(unistd::getpid(), Signal::SIGSTOP);
                let _ = kill(child, Signal::SIGCONT);
            }
            Ok(WaitStatus::Signaled(_, signal, _)) => {
                kill(unistd::getpid(), signal).unwrap_or_else(|err| {
                    panic!("failed to send {} signal to our self: {}", signal, err)
                });
            }
            Ok(WaitStatus::Exited(_, status)) => {
                exit_status = status;
                break;
            }
            Ok(what) => {
                eprintln!("unexpected wait event happend: {:?}", what);
                break;
            }
            Err(e) => {
                eprintln!("waitpid failed: {}", e);
                break;
            }
        };
    }

    fs::remove_dir_all(rootdir)
        .unwrap_or_else(|err| panic!("cannot remove tempdir {}: {}", rootdir.display(), err));

    process::exit(exit_status);
}

// get the nix chroot dir implied by the environment
fn get_implicit_nixdir<'a>() -> PathBuf {

    // if the user specified the location explicitly, use that
    match env::var("NIX_USER_CHROOT_DIR") {
        Ok(val) => if val != "" { return PathBuf::from(val); },
        Err(_) => (),
    }

    // otherwise, use an XDG-friendly user-specific location
    let xdg_dirs = xdg::BaseDirectories::new().unwrap_or_else(|err| {
        panic!("Error getting XDG base dirs: {}", err);
    });
    let nixdir = xdg_dirs.get_data_home().as_path().join("nix-user-chroot");
    if !nixdir.exists() {
        eprintln!("Error: NIX_USER_CHROOT_DIR not defined, and \
                  XDG_DATA_HOME/nix-user-chroot ({}) does not exist. \
                  Please specify the chroot dir for multicall functionality",
                  nixdir.to_str().unwrap());
        process::exit(2);
    }

    return nixdir;
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let nix_commands = [
        "nix",
        "nix-build",
        "nix-channel",
        "nix-collect-garbage",
        "nix-copy-closure",
        "nix-daemon",
        "nix-env",
        "nix-hash",
        "nix-instantiate",
        "nix-prefetch-url",
        "nix-shell",
        "nix-store",
    ];

    let rootdir = mkdtemp::mkdtemp("nix-chroot.XXXXXX")
        .unwrap_or_else(|err| panic!("failed to create temporary directory: {}", err));

    // get the multicall nix command name specified by args[0].
    // returns None if not applicable.
    let cmd: Option<String> =
        Path::new(&args[0]).file_name().and_then(|x| x.to_str())
        .and_then(|x| nix_commands.iter().find(|&&y| x == y))
        .map(|x| x.to_string());

    let (nixdir, args) = match cmd {
        // if called as a nix command (as a busybox-style multicall binary),
        // args are passed along to the command, and the nixdir is implicit.
        Some(cmd) => {
            (get_implicit_nixdir(), [&[cmd][..], &args[1..]].concat())
        }
        // regular non-multicall form
        None => {
            if args.len() < 3 {
                eprintln!("Usage: {} <nixpath> <command>\n", args[0]);
                process::exit(1);
            }

            let nixdir = fs::canonicalize(&args[1])
                .unwrap_or_else(|err| panic!("failed to resolve nix directory {}: {}",
                                             &args[1], err));
            (nixdir, args[2..].to_vec())
        }
    };

    match unsafe { fork() } {
        Ok(ForkResult::Parent { child, .. }) => wait_for_child(&rootdir, child),
        Ok(ForkResult::Child) => RunChroot::new(&rootdir).run_chroot(&nixdir, &args[0], &args[1..]),
        Err(e) => {
            eprintln!("fork failed: {}", e);
        }
    };
}
