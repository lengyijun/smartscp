use crate::error::Error;
use crate::Connection;
use anyhow::Result;
use futures::future::join_all;
use futures::stream::{self, StreamExt};
use git2::Repository;
use git2::Signature;
use git2::Time;
use openssh::Session;
use openssh_sftp_client::metadata::Permissions;
use openssh_sftp_client::Sftp;
use std::ops::DerefMut;
use std::os::unix::prelude::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use std::{thread, time::Duration};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::runtime::Runtime;
use walkdir::WalkDir;

pub struct Uploader<'a> {
    pub c: Connection,
    pub sess: Session,
    pub sftp: &'a Sftp,
    pub rt: Runtime,
}

impl<'a> Uploader<'a> {
    pub fn upload(&mut self) -> Result<(), Error> {
        println!("upload {:?} ", self.c);
        let remote_dir_filestat = match self.rt.block_on(self.sftp.open(&*self.c.remote_path)) {
            Ok(mut file) => self.rt.block_on(file.metadata()).map(|x| x.file_type()),
            Err(e) => Err(e),
        };

        if remote_dir_filestat.is_err() {
            let mut v: Vec<_> = self.c.remote_path.ancestors().skip(1).collect();
            v.reverse();
            for p in v {
                let _ = self.rt.block_on(self.sftp.fs().create_dir(p));
            }
        }

        if self.c.local_path.is_dir() {
            match remote_dir_filestat {
                Ok(Some(stat)) => {
                    if !stat.is_dir() {
                        panic!("remote path is not a dir");
                    }
                    match &mut self.c.remote_path {
                        crate::PathProvenance::Inferred(x) => {
                            panic!("remote destination {:?} already exists, please provide another destination", x)
                        }
                        crate::PathProvenance::UserProvided(p) => {
                            p.push(self.c.local_path.file_name().unwrap());
                        }
                    }
                    self.rt
                        .block_on(self.sftp.fs().create_dir(&*self.c.remote_path))
                        .unwrap();
                }
                _ => {
                    self.rt
                        .block_on(self.sftp.fs().create_dir(&*self.c.remote_path))
                        .unwrap();
                }
            }
            self.upload_dir(&self.c.local_path.clone())?;
            Ok(())
        } else {
            if let Ok(Some(stat)) = remote_dir_filestat {
                if stat.is_dir() {
                    self.c
                        .remote_path
                        .push(self.c.local_path.file_name().unwrap());
                }
            }
            self.rt
                .block_on(self.upload_file(&self.c.local_path.clone(), &self.c.remote_path.clone()))
                .unwrap();
            Ok(())
        }
    }

    pub async fn touch_or_mkdir(&self, entry: walkdir::DirEntry) -> Result<(), Error> {
        if entry.path().is_dir() {
            let x = &self.c.calculate_remote_path(entry.path());
            let _ = self.sftp.fs().create_dir(x).await;
            Ok(())
        } else {
            self.upload_file_2(entry.path()).await
        }
    }

    pub async fn upload_file_2(&self, local_path: &Path) -> Result<(), Error> {
        let remote_path = self.c.calculate_remote_path(local_path);
        self.upload_file(local_path, &remote_path).await
    }

    pub async fn upload_file(&self, local_path: &Path, remote_path: &Path) -> Result<(), Error> {
        let mut file = File::open(local_path)
            .await
            .expect(&format!("unable to open local file: {:?}", local_path));

        println!("{:?}", local_path.file_name().unwrap());
        let permissions = file.metadata().await.unwrap().permissions().mode() & 0o777;

        let mut v = vec![];
        file.read_to_end(&mut v).await.unwrap();

        let mut f = self
            .sftp
            .options()
            .create(true)
            .write(true)
            .open(&remote_path)
            .await
            .map_err(|e| Error(PathBuf::from(remote_path), e))?;

        // write first, then set permission
        // permission maybe readonly
        f.write_all(&v)
            .await
            .map_err(|e| Error(PathBuf::from(remote_path), e))?;

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

        f.set_permissions(perm)
            .await
            .map_err(|e| Error(PathBuf::from(remote_path), e))?;

        f.sync_all()
            .await
            .map_err(|e| Error(PathBuf::from(remote_path), e))
    }

    fn upload_file_or_dir(&self, entry: &Path) -> Result<(), Error> {
        if entry.is_dir() {
            self.upload_dir(entry)
        } else {
            let remote_path = self.c.calculate_remote_path(entry);
            self.rt
                .block_on(async { self.upload_file(entry, &remote_path).await })
        }
    }

    fn upload_dir(&self, path: &Path) -> Result<(), Error> {
        match Repository::open(path) {
            Ok(repo) => self.upload_git_dir(repo),
            Err(_) => {
                let walker = walkdir::WalkDir::new(path).max_depth(1);
                for entry in walker {
                    let entry = entry.unwrap();
                    if entry.path() == path {
                        continue;
                    }
                    self.upload_file_or_dir(entry.path())?;
                }
                Ok(())
            }
        }
    }

    // upload a dir, without checking git, recursively
    async fn upload_non_git_dir(&self, path: &Path) -> Result<(), Error> {
        let mut v = vec![];
        let walkdir = WalkDir::new(path);
        for entry in walkdir {
            let entry = entry.unwrap();
            v.push(self.touch_or_mkdir(entry).await);
        }

        // let stream = join_all(v).await;

        //TODO: remove
        for x in v.into_iter() {
            match x {
                Ok(()) => {}
                Err(e) => {
                    eprintln!("{:?}", e);
                }
            }
        }
        Ok(())
    }

    fn upload_git_dir(&self, repo: Repository) -> Result<(), Error> {
        let x = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

        let x = self
            .rt
            .block_on(async { self.upload_non_git_dir(repo.path()).await.unwrap() });

        let mut path_buf = PathBuf::from(repo.path());
        path_buf.pop();
        let path_buf = path_buf;
        let remote_path = self.c.calculate_remote_path(&path_buf);
        let x = self
            .rt
            .block_on(async {
                self.sess
                    .command("sh")
                    .arg("-c")
                    .arg(&format!(
                        "cd {} && git checkout . && git submodule update --init --recursive",
                        remote_path.to_string_lossy()
                    ))
                    .spawn()
                    .await
            })
            .unwrap();

        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true);
        // don't deal with submodule because submodules status is complicated
        opts.exclude_submodules(true);

        // on my Ubuntu, must wait for a second
        // on manjaro, don't need to wait
        thread::sleep(Duration::from_secs(1));

        // list untracked files, modifed files and remove removed files
        if let Ok(untracked_files) = repo.statuses(Some(&mut opts)) {
            for entry in untracked_files.iter() {
                if entry.status().contains(git2::Status::IGNORED) {
                    continue;
                }
                let path = path_buf.join(entry.path().unwrap());
                if entry.status().contains(git2::Status::WT_DELETED) {
                    let x = self
                        .rt
                        .block_on(async {
                            self.sess
                                .command("sh")
                                .arg("-c")
                                .arg(&format!("rm -rf {}", path.to_string_lossy(),))
                                .spawn()
                                .await
                        })
                        .unwrap();
                } else if entry.status().contains(git2::Status::WT_NEW)
                    || entry.status().contains(git2::Status::WT_MODIFIED)
                {
                    self.upload_file_or_dir(&path)?;
                }
            }
        }

        Ok(())
    }
}
