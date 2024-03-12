use anyhow::Context;
use anyhow::Result;
use crossbeam_channel as cbc;
use log::error;
use log::info;
use pathdiff::diff_paths;
use ssh2_config::SshConfig;
use ssh2_config::{HostParams, ParseRule};
use std::collections::HashMap;
use std::env;
use std::io::BufReader;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use xcp::drivers::load_driver;
use xcp::errors::XcpError;
use xcp::operations::StatSender;
use xcp::operations::StatusUpdate;

#[derive(Debug)]
pub enum PathProvenance {
    Inferred(PathBuf),
    UserProvided(PathBuf),
}

impl Deref for PathProvenance {
    type Target = PathBuf;

    fn deref(&self) -> &Self::Target {
        match self {
            PathProvenance::Inferred(p) => p,
            PathProvenance::UserProvided(p) => p,
        }
    }
}

impl DerefMut for PathProvenance {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            PathProvenance::Inferred(p) => p,
            PathProvenance::UserProvided(p) => p,
        }
    }
}

#[derive(Debug)]
pub struct Connection {
    // always save an absolute path
    remote_path: PathProvenance,
    // always save an absolute path
    local_path: PathBuf,
}

impl Connection {
    fn new(remote_path: Option<&str>, local_path: &str, remote_home: Option<String>) -> Self {
        let mut local_path_pf: PathBuf =
            match shellexpand::full(local_path).map(|x| Path::new(x.as_ref()).canonicalize()) {
                Ok(Ok(x)) => x,
                _ => panic!("not a valid local path"),
            };
        if local_path_pf.is_relative() {
            local_path_pf = env::current_dir().unwrap().join(local_path_pf);
        }
        let remote_path_pf = match remote_path {
            Some(x) => {
                let pf =
                    PathBuf::from(shellexpand::tilde_with_context(&x, || remote_home).as_ref());
                PathProvenance::UserProvided(pf)
            }
            None => {
                let pf = match diff_paths(&local_path_pf, std::env::var("HOME").unwrap()) {
                    Some(x) => PathBuf::from(&remote_home.unwrap()).join(x),
                    None => panic!("don't support upload to remote path other than home"),
                };
                PathProvenance::Inferred(pf)
            }
        };

        assert!(remote_path_pf.is_absolute());
        assert!(local_path_pf.is_absolute());

        Connection {
            remote_path: remote_path_pf,
            local_path: local_path_pf,
        }
    }
}

enum Direction {
    Upload,
    Download,
}

fn main() -> Result<()> {
    let mut arg_iter = env::args().skip(1);
    let arg1: String = arg_iter.next().unwrap();
    let arg2: String = arg_iter.next().unwrap();
    if arg_iter.next().is_some() {
        panic!("expect 2 arguments, meet more than 2");
    }
    let arg1_split = arg1.split_once(':');
    let arg2_split = arg2.split_once(':');

    let (remote_host, remote_path, local_path, direction) = match (arg1_split, arg2_split) {
        (None, None) => {
            // scp local_path remote-host
            // ok
            (arg2, None, arg1, Direction::Upload)
        }
        (None, Some((remote_host, remote_path))) => {
            // scp local_path remote-host:remote-path
            // ok
            (
                remote_host.to_owned(),
                Some(remote_path),
                arg1,
                Direction::Upload,
            )
        }
        (Some((remote_host, remote_path)), None) => {
            // scp remote-host:remote-path local_path
            // ok
            (
                remote_host.to_owned(),
                Some(remote_path),
                arg2,
                Direction::Download,
            )
        }
        (Some(_), Some(_)) => {
            unimplemented!("don't support filename contains :")
        }
    };

    let host_params = get_remote_host(&remote_host).unwrap();

    let mount = tempfile::tempdir()?;

    let connection = Connection::new(
        remote_path,
        &local_path,
        host_params.user.map(|u| format!("/home/{u}")),
    );
    let remote_path = mount
        .path()
        .join(diff_paths(&*connection.remote_path, "/").unwrap());

    Command::new("sshfs")
        .arg(format!("{remote_host}:/"))
        .arg(mount.path())
        .status()
        .context("Fail to execute `sshfs`, maybe `sshfs` not found ?")?;

    let opts = Arc::new(xcp::options::Opts {
        gitignore: true,
        recursive: true,
        fsync: true,
        verbose: 0,
        workers: 4,
        block_size: 1048576,
        no_clobber: false,
        glob: false,
        no_progress: false,
        no_perms: false,
        driver: xcp::drivers::Drivers::ParFile,
        no_target_directory: false,
        reflink: xcp::operations::Reflink::Auto,
        paths: vec![],
    });

    match direction {
        Direction::Upload => {
            println!("local: {:?}", connection.local_path);
            println!("remote: {:?}", connection.remote_path.deref());
        }
        Direction::Download => {
            println!("remote: {:?}", connection.remote_path.deref());
            println!("local: {:?}", connection.local_path);
        }
    }

    let (source, dest): (PathBuf, PathBuf) = match direction {
        Direction::Upload => (connection.local_path, remote_path),
        Direction::Download => (remote_path, connection.local_path),
    };

    let pb = xcp::progress::create_bar(&opts, 0)?;
    let (stat_tx, stat_rx) = cbc::unbounded();
    let stats = StatSender::new(stat_tx, &opts);

    let driver = load_driver(&opts)?;

    if dest.is_file() {
        // Special case; attemping to rename/overwrite existing file.
        if opts.no_clobber {
            return Err(XcpError::DestinationExists(
                "Destination file exists and --no-clobber is set.",
                dest,
            )
            .into());
        }

        /*
        // Special case: Attempt to overwrite a file with
        // itself. Always disallow for now.
        if is_same_file(&source, &dest)? {
            return Err(XcpError::DestinationExists(
                "Source and destination is the same file.",
                dest,
            )
            .into());
        }
         */

        info!("Copying file {:?} to {:?}", source, dest);
        driver.copy_single(&source, &dest, stats)?;
    } else {
        // Sanity-check all sources up-front
        info!("Copying source {:?} to {:?}", source, dest);
        if !source.exists() {
            return Err(XcpError::InvalidSource("Source does not exist.").into());
        }

        if source.is_dir() && !opts.recursive {
            return Err(XcpError::InvalidSource(
                "Source is directory and --recursive not specified.",
            )
            .into());
        }

        if &source == &dest {
            return Err(XcpError::InvalidSource("Cannot copy a directory into itself").into());
        }

        if dest.exists() && !dest.is_dir() {
            return Err(XcpError::InvalidDestination(
                "Source is directory but target exists and is not a directory",
            )
            .into());
        }

        driver.copy_all(vec![source], &dest, stats)?;
    }

    // Gather the results as we go; our end of the channel has been
    // moved to the driver call and will end when drained.
    for stat in stat_rx {
        match stat {
            StatusUpdate::Copied(v) => pb.inc(v),
            StatusUpdate::Size(v) => pb.inc_size(v),
            StatusUpdate::Error(e) => {
                // FIXME: Optional continue?
                error!("Received error: {}", e);
                return Err(e.into());
            }
        }
    }

    Command::new("umount").arg(mount.path()).status()?;
    pb.end();
    Ok(())
}

fn get_remote_host(remote_host: &str) -> Result<HostParams> {
    let param = match remote_host.split_once(|x| x == '@') {
        Some((user_name, ip)) => HostParams {
            bind_address: None,
            bind_interface: None,
            ca_signature_algorithms: None,
            certificate_file: None,
            ciphers: None,
            compression: None,
            connection_attempts: None,
            connect_timeout: None,
            host_key_algorithms: None,
            host_name: Some(ip.to_owned()),
            identity_file: None,
            ignore_unknown: None,
            kex_algorithms: None,
            mac: None,
            port: None,
            pubkey_accepted_algorithms: None,
            pubkey_authentication: None,
            remote_forward: None,
            server_alive_interval: None,
            tcp_keep_alive: None,
            user: Some(user_name.to_owned()),
            ignored_fields: HashMap::new(),
        },
        None => {
            let ssh_config_location: PathBuf = [&std::env::var("HOME").unwrap(), ".ssh", "config"]
                .iter()
                .collect();

            let mut reader = BufReader::new(
                std::fs::File::open(ssh_config_location)
                    .expect("Could not open configuration file"),
            );
            let config = SshConfig::default()
                .parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS)
                .expect("Failed to parse configuration");

            // Query attributes for a certain host
            config.query(remote_host)
        }
    };
    Ok(param)
}
