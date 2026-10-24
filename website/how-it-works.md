# How unpakt works

This page explains what unpakt does on your computer, in plain terms. You do
not need to know any programming.

## What happens when you run `unpakt add https://github.com/owner/repo`

1. **unpakt finds the app.** It asks GitHub for the repository's latest
   release and looks at the files attached to it.
2. **It picks the right file.** It ignores checksums, signatures and files for
   other systems or processors (Windows, macOS, 32-bit, ARM). Among what is
   left it prefers an AppImage built for x86_64 Linux, then a tarball.
3. **It downloads the file** to a temporary folder in `~/.cache/unpakt`.
4. **It looks inside, without running anything.** unpakt reads the file to find
   the app's name, version, icon, launcher information and the program to
   start. Nothing from the download is executed at this point.
5. **It shows you a summary** (name, version, where it came from, the command
   it will create) and asks `Install? [y/N]`. Nothing has been changed yet. You
   can see the same summary with `unpakt inspect`, which never installs.
6. **It installs.** The app goes into its own folder, a launcher entry and an
   icon are added so it shows up in your applications menu, and a command is
   placed in `~/.local/bin` so you can start it from a terminal.

If you give a download URL or a file instead, the steps are the same minus the
GitHub lookup.

## Where things go

Everything is inside your home directory.

| What | Where |
| --- | --- |
| The apps | `~/.local/share/unpakt/apps/<app>/<version>/` |
| unpakt's records | `~/.local/share/unpakt/db.sqlite` |
| Menu entries | `~/.local/share/applications/unpakt-<app>.desktop` |
| Icons | `~/.local/share/icons/hicolor/` |
| Terminal commands | `~/.local/bin/` |
| Temporary downloads | `~/.cache/unpakt/downloads/` |

`~/.local/bin` has to be on your `PATH` for the terminal command to work.
`unpakt doctor` tells you whether it is.

unpakt keeps a list of every file it created for each app. Removing an app
deletes exactly those files. Files that unpakt did not create are never
replaced unless you pass `--force`, and a command that would be hidden by
another program of the same name is refused.

## Updates

unpakt never checks for updates by itself. When you run
`unpakt update --check`, it asks each app's source what the newest version is:
for a GitHub repo it compares release tags; for a download URL it compares the
file's `ETag` or `Last-Modified` header. Apps installed from a file on your
disk have nowhere to ask and are skipped.

`unpakt update` installs the new version next to the old one and only then
switches the app over. If anything fails or you press Ctrl-C part-way, the old
version stays exactly as it was. unpakt keeps the last three versions of each
app.

## Rollbacks

`unpakt rollback <app>` switches the app back to the previous version. It is
quick, since the older version is still on disk, and it deletes nothing. The
menu entry and icon stay; only what they launch changes. Running
For a GitHub app, `unpakt update` afterwards moves forward again without
downloading that version a second time.

## If the power goes out mid-install

Each install, update, rollback and removal writes a small journal of what it is
about to do before it does it. If the computer loses power or the process is
killed:

- You may see leftovers from the interrupted step, but the app that was
  working before still works, because the new version was never switched on.
- The next time you run a command that changes something (`add`, `update`,
  `rollback` or `remove`), unpakt reads the journal first. If the interrupted
  change had actually completed, it only cleans up. Otherwise it undoes the
  steps one by one.
- If a journal is damaged or points somewhere unpakt does not manage, unpakt
  stops and tells you which file to look at instead of guessing.
  `unpakt doctor` lists such files.

Only one unpakt changes things at a time. If you start a second one (or use
the GUI while a command is running), it waits for the first and then goes on.

## What unpakt never touches

- It never asks for root and never uses `sudo`.
- It writes only under your home directory: `~/.local/share`, `~/.local/bin`
  and `~/.cache`. It does not touch `/usr`, `/opt` or `/etc`.
- It does not run the apps it installs, other than asking the desktop to
  refresh its menu (`update-desktop-database`, if you have it).
- It does not run in the background and does not start with your session.
- It does not verify the signatures of the apps it installs yet, so install
  apps from sources you trust.

If you remove unpakt itself, the apps it installed stay and keep working. Use
`unpakt self uninstall --purge` if you want unpakt to remove them as well.
