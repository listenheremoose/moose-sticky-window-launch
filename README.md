# moose-sticky-window-launch

**Make apps reopen on the same screen and workspace you last used them on.**

By default, the COSMIC desktop on Pop!_OS opens every app on whatever workspace
you're looking at. This tool remembers where each app was when you closed it.
The next time you open that app, it moves the window back there for you.

For example, if you keep your browser on workspace 2 and your chat app on
workspace 3 of your second monitor, they'll go back to those spots every time
you open them, including after a restart.

It runs quietly in the background and starts on its own each time you log in.

---

## What you need

- **Pop!_OS 24.04 or newer with the COSMIC desktop.** It won't work on GNOME,
  KDE, or other desktops.
- **Rust**, the programming language this tool is written in, which you need
  to build it. Check if you already have it by opening a terminal and typing:

  ```sh
  cargo --version
  ```

  If you see a version number, you're set. If you get "command not found",
  install Rust with this command and follow the prompts (the defaults are
  fine):

  ```sh
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

  Then **close the terminal and open a new one** so it picks up the change.

> **New to the terminal?** Open the app called **Terminal** (COSMIC Terminal).
> You can paste a command into it with **Ctrl + Shift + V** (plain Ctrl + V
> doesn't paste in a terminal), then press **Enter** to run it.

---

## Installing

1. In the terminal, go to this project's folder. If it's where this README
   lives, for example:

   ```sh
   cd ~/code/moose-sticky-window-launch
   ```

2. Run the installer:

   ```sh
   ./install.sh
   ```

   The first time, this takes a minute or two while it builds. When it's done
   you'll see `Installed.`

That's it. It's now running, and it will start by itself every time you log in.
You don't need to keep the terminal open.

### Checking that it works

1. Open an app, for example the Text Editor.
2. Move it to a different workspace.
3. Close it.
4. Open it again. It should jump to the workspace you left it on.

Want to watch what it's doing? Run this, then open and close some apps:

```sh
journalctl --user -u moose-sticky-window-launch -f
```

Each time a window opens, it prints a line saying where it moved the window,
or why it left it alone. Press **Ctrl + C** to stop watching. That only stops
the log; the tool keeps running.

---

## Turning it on and off

| I want to… | Command |
|---|---|
| Pause it until I next log in | `systemctl --user stop moose-sticky-window-launch` |
| Start it again | `systemctl --user start moose-sticky-window-launch` |
| Turn it off for good (stays off after logging in) | `systemctl --user disable --now moose-sticky-window-launch` |
| Turn it back on for good | `systemctl --user enable --now moose-sticky-window-launch` |
| Check if it's running | `systemctl --user status moose-sticky-window-launch` |

Turning it off doesn't erase anything. It remembers everything when you turn it
back on.

---

## How it decides where windows go

- **It remembers each app, not each window.** Wherever you last closed an app
  is where it opens next time.
- **Only the app's first window is moved.** If an app is already open,
  new windows from it (like a second browser window or a "Save as" dialog)
  open normally, so they don't jump away from you.
- **Windows that were already open when it started are left alone.**
- **If the screen isn't connected** (say you saved a spot on your external
  monitor but you're on just your laptop now), the window opens normally.
- **If the workspace is gone.** COSMIC deletes empty workspaces on its own,
  so the one you used may not exist anymore. In that case the window goes to
  a fresh workspace at the end of that screen. That may not be the same number
  as before. For example, if it was on workspace 5 but you now only have 1 and
  2, it lands on 3.
- **Plugging in or unplugging a monitor won't mess up your saved spots.** For
  10 seconds afterward, the tool ignores window movement while COSMIC
  rearranges things.
- **Windows set to show on all workspaces** are ignored.
- **You may see the window flash briefly** on your current workspace before it
  moves. On Linux's modern display system (Wayland), a tool like this can only
  move a window after the window has appeared, so this is normal.

> **Tip:** If you want apps to always land on the exact same workspace number,
> **pin** your workspaces in COSMIC. Pinned workspaces are never deleted.

---

## Settings (optional)

You don't need to change any settings. If you want to, create this file:

```
~/.config/moose-sticky-window-launch/config.json
```

(`~` means your home folder. `.config` is a hidden folder, so in the Files app
press **Ctrl + H** to show hidden files.)

Put this in it and change whatever you like:

```json
{
  "exclude": [],
  "follow": false,
  "create_missing_workspace": true
}
```

| Setting | What it does |
|---|---|
| `exclude` | Apps the tool should never move or remember. See below for how to find an app's name. Example: `["com.system76.CosmicTerm", "firefox"]` |
| `follow` | `true` = when an app gets moved, take you to that workspace too. `false` (default) = you stay where you are and the app opens on its own workspace. |
| `create_missing_workspace` | `true` (default) = if the saved workspace is gone, use a fresh one at the end. `false` = just open the app normally. |

After saving the file, restart the tool so it reads your changes:

```sh
systemctl --user restart moose-sticky-window-launch
```

### Finding an app's name

Apps have an internal name (called an "app id") that's often different from
what you see on screen. With the app open, run:

```sh
moose-sticky-window-launch --dump
```

Look under **Windows:**. The left column is the app id, and the right side
shows which screen and workspace it's on.

---

## Uninstalling

From this project's folder:

```sh
./uninstall.sh
```

This stops the tool and removes it. Your saved window spots are kept in
`~/.local/state/moose-sticky-window-launch/` in case you reinstall. Delete that
folder if you want them gone too.

---

## Troubleshooting

**`./install.sh: Permission denied`.** Run `chmod +x install.sh uninstall.sh`
first, then try again.

**`cargo: command not found`.** Rust isn't installed, or you didn't open a new
terminal after installing it. See [What you need](#what-you-need).

**`moose-sticky-window-launch: command not found`** when running `--dump`.
Open a new terminal, or use the full path:
`~/.cargo/bin/moose-sticky-window-launch --dump`.

**An app doesn't move.** Watch the log (see [Checking that it works](#checking-that-it-works))
while opening it. It tells you why, for example "another window of this app
is already open" or "no remembered placement" (it hasn't seen you close that
app yet).

**It says "compositor does not offer…".** You're not on COSMIC, or the tool
is running inside a sandbox like Flatpak. It needs to run directly on a COSMIC
desktop.

**I want to reset everything.** Stop the tool, delete
`~/.local/state/moose-sticky-window-launch/placements.json`, then start it
again.

---

## Technical details

For the curious. You don't need this to use the tool.

- Written in Rust and runs as a systemd user service
  (`~/.config/systemd/user/moose-sticky-window-launch.service`).
- Saved spots live in `~/.local/state/moose-sticky-window-launch/placements.json`,
  keyed by Wayland app id. Monitors are matched by connector name (`DP-2`,
  `HDMI-A-1`, …), and workspaces by id, then position, then name.
- Wayland protocols used: `ext_foreign_toplevel_list_v1`,
  `zcosmic_toplevel_info_v1` (v3), `zcosmic_toplevel_manager_v1` (v4,
  `move_to_ext_workspace`), `ext_workspace_manager_v1`, `wl_output` (v4).
  COSMIC only offers these to unsandboxed programs.
- cosmic-comp currently ignores `ext_workspace_group_handle_v1.create_workspace`,
  so the "missing workspace" fallback moves the window onto the trailing empty
  workspace that COSMIC always keeps, and COSMIC then adds another one.
