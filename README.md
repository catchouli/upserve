# upserve

<a href="screenshots/listing.jpg"><img src="screenshots/listing-thumb.jpg" width="32%"></a>
<a href="screenshots/uploading.jpg"><img src="screenshots/uploading-thumb.jpg" width="32%"></a>
<a href="screenshots/drop.jpg"><img src="screenshots/drop-thumb.jpg" width="32%"></a>

A tiny file server for sharing files on your local network. Run it in a folder and
everyone on the network gets a plain directory listing they can download from, just like
`python -m http.server`, except they can upload files and whole folders back to you too.

I use `http.server` all the time for passing files around our LAN, and for a while I was
using [uploadserver](https://pypi.org/project/uploadserver/) to get uploads as well. It
works, but it needs an older version of Python than I've got, and it can't upload
folders, which is usually exactly what I want to send. So this is that, in Rust, as a
single binary with nothing to install alongside it.

## What's in it

**Uploading** is right there on the listing page: choose some files or a folder and press
Upload, or just drag files and folders onto the page and they go straight up. You can
drop files and folders together, and the folder structure is kept (empty folders too,
when you drag them in). Everything goes into the folder you're looking at, and the
listing updates when it's done.

**Big files** are fine. They're sent in 8 MB chunks, so a dropped connection only costs
you the chunk it was in (it gets retried), and there's no upload size limit. While a file
is uploading it goes into a hidden temporary file next to where it's going, and only gets
renamed to its real name once every byte has arrived, so nobody ever downloads half a
file. Files keep their original modified dates, too.

**Nothing gets overwritten.** If a file with the same name is already there, the upload
is refused before it starts, unless you run it with `--allow-replace`. Folders are never
replaced either way.

**Downloads** work the same as `http.server`, plus they can be resumed, since range
requests are supported.

## Running it

You'll need [Rust](https://rustup.rs). Then:

```sh
cargo install --git https://github.com/catchouli/upserve
```

And in whichever folder you want to share:

```sh
upserve
```

It listens on port 8000 on every network interface, just like `http.server`. The options
are:

```
upserve [PORT]                  port to listen on (default 8000)
  -b, --bind <BIND>             address to listen on (default 0.0.0.0)
  -d, --directory <DIRECTORY>   folder to share (default: the current one)
      --allow-replace           let uploads replace existing files
```

## Is it safe?

It's meant for networks you trust. There are no passwords, so anyone who can reach it can
download everything in the folder and upload whatever they like into it.

What it does make sure of is that nobody can get outside the folder you're sharing:
paths with `..` in them are refused, and uploads won't follow a symlink that points
outside it. Symlinks you've made yourself still work for downloads, the same as with
`http.server`.

One rough edge: if someone closes the page halfway through an upload, the hidden
`.upserve-*.part` file it was writing to gets left behind. They don't show up in the
listing, but you might want to clear them out now and then.
