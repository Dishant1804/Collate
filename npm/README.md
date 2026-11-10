# collate-app

Installs [Collate](https://github.com/Dishant1804/Collate), a fast git diff viewer for macOS.

```sh
npx collate-app@latest
```

It checks the Mac (macOS 13 or newer), installs Xcode's command line tools if git is
missing, downloads the newest Collate.app into Applications and adds a `collate` command.
Run the same command again to update.

- `--build`: build the latest source instead (installs Rust if it is missing).
- `--uninstall`: remove the app and the command.
