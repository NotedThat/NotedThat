# Mounting a knowledge base as a folder

Every knowledge base is a directory tree of ordinary files, and the server serves that tree over
WebDAV at `/webdav`. Mount it and it is a folder: open notes in whatever editor you already use,
drag files in from a file manager, `grep` it, point an Obsidian vault at it. What the folder
cannot do on its own — search across it, hand a slice of it to an agent, let a colleague write to
one subtree and not another — the server does for it. Anything written through the mount is
indexed and searchable shortly after, and anything an agent writes through
[MCP](CLIENTS.md) shows up in the folder.

The protocol reference — methods, error conditions, path rules — is under
[WebDAV](API.md#webdav). This page is how to mount it.

## The address and the credential

```
https://notes.example.com/webdav/           ← root: one directory per knowledge base
https://notes.example.com/webdav/notes/     ← the `notes` knowledge base
```

Sign in with HTTP Basic using `NOTEDTHAT_WEBDAV_USERNAME` / `NOTEDTHAT_WEBDAV_PASSWORD`. That
credential is the deployment's own — it resolves to the same principal as `NOTEDTHAT_API_TOKEN`,
so it sees everything `signed-in` rules grant. WebDAV also accepts `Authorization: Bearer`, which
is how a client with a per-person [identity token](CONFIGURATION.md#oidc-authentication) mounts as
one person, bound by that person's `group:` and `user:` rules; rclone below can do this. Basic
auth over plain HTTP is only for loopback — a public server sits behind TLS.

## One thing to know first: no `LOCK`

NotedThat is a WebDAV Class 1 server; it does not implement `LOCK`/`UNLOCK`. Most clients never
ask for a lock and are unaffected. A few insist on one before they will save:

- **macOS Finder** mounts and reads, but saves fail. Use rclone on macOS for read-write.
- **Microsoft Office** opening a file straight from the share cannot save back.

Everything below is known to work read-write.

## Linux

**GNOME Files (Nautilus)** — *Other Locations* → *Connect to Server*, or `Ctrl+L` in any
window:

```
davs://notes.example.com/webdav/
dav://localhost:8080/webdav/          # plain HTTP, local server only
```

**KDE Dolphin** — the same, with `webdavs://` (or `webdav://`) in the address bar.

Both give you a folder under `/run/user/<uid>/gvfs/` or `kio-fuse`, so command-line tools work on
it too.

**davfs2**, for a real mount point that survives logout and is visible to every process:

```sh
sudo apt install davfs2                  # or the distribution's equivalent
sudo mkdir -p /mnt/notes
echo 'https://notes.example.com/webdav/ webdav-user webdav-pass' | sudo tee -a /etc/davfs2/secrets
sudo chmod 600 /etc/davfs2/secrets
```

Tell davfs2 not to ask for locks, in `/etc/davfs2/davfs2.conf` (or `~/.davfs2/davfs2.conf`):

```
use_locks 0
```

Then:

```sh
sudo mount -t davfs https://notes.example.com/webdav/ /mnt/notes
```

Or in `/etc/fstab`, so a user can mount it without `sudo`:

```
https://notes.example.com/webdav/  /mnt/notes  davfs  user,noauto,rw  0  0
```

davfs2 keeps a local cache and uploads a file a few seconds after it is closed, so an edit is
searchable a moment after you save, not the instant you do.

## rclone — every platform, and the answer to "sync"

[rclone](https://rclone.org) has a WebDAV backend, works on Linux, macOS and Windows, and does not
need `LOCK`. One remote gives you a mount, one-way sync, and a copy command:

```sh
rclone config create notedthat webdav \
  url=https://notes.example.com/webdav/ vendor=other \
  user=webdav-user pass=webdav-pass --obscure
```

To mount as one person rather than as the service credential, give it an identity token instead
of the Basic pair:

```sh
rclone config create notedthat webdav \
  url=https://notes.example.com/webdav/ vendor=other \
  bearer_token="$IDENTITY_TOKEN"
```

Then:

```sh
rclone lsd notedthat:                              # the knowledge bases
rclone mount notedthat:notes ~/Notes --vfs-cache-mode writes   # a folder (macOS: macFUSE; Windows: WinFsp)
rclone copy ~/Documents/reading-list.md notedthat:notes/inbox/  # drop a file in
rclone sync notedthat:notes ~/Backups/notes                     # pull a copy out
```

`--vfs-cache-mode writes` is what lets editors that write files in several steps (rename over a
temporary file, truncate then write) work over WebDAV.

## Windows

**WinSCP** — new session, protocol *WebDAV*, encryption *TLS/SSL Implicit encryption*, host
`notes.example.com`, remote directory `/webdav/`, and the Basic username and password. It browses
and edits files in place.

**rclone** as above, with [WinFsp](https://winfsp.dev) installed, for a drive letter:

```powershell
rclone mount notedthat:notes N: --vfs-cache-mode writes
```

## Obsidian, and the "sync folder" question

Point Obsidian at the mounted folder — *Open folder as vault* on `~/Notes` or `/mnt/notes` — and
keep using Obsidian. What changes is what else is true of the vault:

- an assistant with an [MCP client](CLIENTS.md) can search it and write to it, and can be given
  a token that may write only to `inbox/` — Obsidian and the sync folder have no way to say that;
- a teammate can be given `research/**` and nothing else, and sees a vault with only
  `research/` in it;
- `search` is hybrid semantic + keyword over the whole vault, from any client, not only the app;
- every change, from any surface, is on an [event stream](API.md#get-apiv1knowledgebaseskb_slugevents)
  a workflow can subscribe to.

A vault that is also a mount is still one folder: Obsidian's own indexing runs against the mount
and can be slow over a remote link on a large vault. rclone's `--vfs-cache-mode full` keeps a local
copy of what has been read, which usually makes the difference.

## Or skip the mount entirely

A server running the [filesystem storage backend](CONFIGURATION.md#filesystem-storage-backend)
stores objects as files under `NOTEDTHAT_FS_ROOT`, at their key paths, and watches that tree.
On the machine the server runs on, that directory *is* the knowledge base — edit it directly,
back it up with any file-level tool — and a change made that way is picked up and re-indexed.
WebDAV is for everyone who is not on that machine, and for the S3 backend, which has no
directory to open.
