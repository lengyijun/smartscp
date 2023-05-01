# Smartscp

A wrapper of scp, auto skip git-ignored files

## Usage
```
smartscp remote-host:path local_path
smartscp local_path remote-host
smartscp local_path remote-host:remote-path
```

## Feature
1. respect git ignore 

git-ignored files will not be scped

2. no `-r` needed in transferring folder

3. auto fill the path
```
# auto complete the destination: `remote_host:~/.local/share`
smartscp ~/.local/share remote_host
```

## Not supported yet
1. use password to authorize
2. filename contains ":"

## Notice
not compatible with scp
not a replacement of scp

## Q&A
### Q: why not use `rsync --exclude=`
A: rsync doesn't support complicated exclude rules

### Q: why not rewrite scp from bottom up ?
A: It's not a trival work

