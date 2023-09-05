use crate::increase_nofile_limit;
use crate::is_gitignore_local;
use crate::Connection;
use futures::stream::{self, StreamExt};
use openssh_sftp_client::metadata::Permissions;
use openssh_sftp_client::Error;
use openssh_sftp_client::Sftp;
use std::os::unix::prelude::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use walkdir::{DirEntry, WalkDir};

pub async fn upload(mut c: Connection, sftp: &Sftp) -> Result<(), Error> {
    println!("upload {:?} ", c);
    let remote_dir_filestat = match sftp.open(&c.remote_path).await {
        Ok(mut file) => file.metadata().await.map(|x| x.file_type()),
        Err(e) => Err(e),
    };

    if remote_dir_filestat.is_err() {
        let mut v: Vec<_> = c.remote_path.ancestors().skip(1).collect();
        v.reverse();
        for p in v {
            let _ = sftp.fs().create_dir(p).await;
        }
    }

    if c.local_path.is_dir() {
        match remote_dir_filestat {
            Ok(Some(stat)) => {
                if !stat.is_dir() {
                    panic!("remote path is not a dir");
                }
                c.remote_path.push(c.local_path.file_name().unwrap());
                // needless to create_dir here
                // we will create_dir in walker
                // let _ = sftp.fs().create_dir(&c.remote_path).await;
            }
            _ => {
                sftp.fs().create_dir(&c.remote_path).await.unwrap();
            }
        }

        let walker = WalkDir::new(&c.local_path).into_iter();
        let mut v = vec![];
        for entry in walker {
            let entry = entry.unwrap();
            v.push(upload_worker(&c, sftp, entry));
        }

        // send too fast will run out of fd
        let soft_limit = increase_nofile_limit()
            .map_or_else(|_| 512, |n| n / 4)
            .min(1024);
        let stream = stream::iter(v)
            .buffered(soft_limit as usize)
            .collect::<Vec<_>>()
            .await;

        for x in stream.into_iter() {
            match x {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{:?}", e);
                }
            }
        }
        Ok(())
    } else {
        if let Ok(Some(stat)) = remote_dir_filestat {
            if stat.is_dir() {
                c.remote_path.push(c.local_path.file_name().unwrap());
            }
        }
        upload_file(sftp, &c.local_path, &c.remote_path)
            .await
            .map_err(|(e, _)| e)
    }
}

pub async fn upload_worker(
    c: &Connection,
    sftp: &Sftp,
    entry: DirEntry,
) -> Result<(), (Error, PathBuf)> {
    if entry.path().is_dir() {
        let _ = sftp
            .fs()
            .create_dir(&c.calculate_remote_path(entry.path()))
            .await;
        Ok(())
    } else if !is_gitignore_local(entry.path()) {
        let remote_path = c.calculate_remote_path(entry.path());
        upload_file(sftp, entry.path(), &remote_path).await
    } else {
        Ok(())
    }
}

pub async fn upload_file(
    sftp: &Sftp,
    local_path: &Path,
    remote_path: &Path,
) -> Result<(), (Error, PathBuf)> {
    println!("{:?}", local_path.file_name().unwrap());
    let mut file = File::open(local_path).await.unwrap();
    let permissions = file.metadata().await.unwrap().permissions().mode() & 0o777;

    let mut v = vec![];
    file.read_to_end(&mut v).await.unwrap();

    let mut f = sftp
        .options()
        .create(true)
        .write(true)
        .open(&remote_path)
        .await
        .map_err(|e| (e, PathBuf::from(remote_path)))?;
    let mut perm = Permissions::new();

    perm.set_read_by_owner((permissions & 0b100_000_000) != 0);
    perm.set_write_by_owner((permissions & 0b010_000_000) != 0);
    perm.set_execute_by_owner((permissions & 0b001_000_000) != 0);

    perm.set_read_by_group((permissions & 0b000_100_000) != 0);
    perm.set_write_by_group((permissions & 0b000_010_000) != 0);
    perm.set_execute_by_group((permissions & 0b000_001_000) != 0);

    perm.set_read_by_other((permissions & 0b000_000_100) != 0);
    perm.set_write_by_other((permissions & 0b000_000_010) != 0);
    perm.set_execute_by_other((permissions & 0b000_000_001) != 0);

    // write first, then set permission
    // permission maybe readonly
    f.write_all(&v)
        .await
        .map_err(|e| (e, PathBuf::from(remote_path)))?;

    f.set_permissions(perm)
        .await
        .map_err(|e| (e, PathBuf::from(remote_path)))
}
