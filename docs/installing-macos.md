# Installing eidos on macOS

macOS 11 or later, Apple silicon or Intel. There is no installer package yet:
you build `Eidos.app` from a clone and install the agent from it.

```bash
git clone https://github.com/josiah-nelson/eidos.git
cd eidos
scripts/macos/build-agent.sh
"dist/macos/Eidos.app/Contents/MacOS/eidos" service install \
    --data-dir ~/Library/Application\ Support/eidos \
    --bind 127.0.0.1:7700 \
    --start-now
```

Then open <http://127.0.0.1:7700> and add a source.

## Why an app bundle for a command-line program

Full Disk Access — the permission that lets the agent read files macOS
protects, which is most of a home directory — is only properly supported for
executables inside an app bundle. A loose binary inherits whatever the process
that launched it was granted, so an agent started by launchd would have none.
Bundling the same binary changes nothing about how it runs and everything
about what you can grant it.

`eidos service install` says so if you point it at a binary outside a bundle.
Everything works; sources under protected folders simply come back empty until
the agent is installed from `Eidos.app`.

To grant it: **System Settings → Privacy & Security → Full Disk Access**, add
`Eidos.app`, then `eidos service restart`. A permission change only takes
effect for a process started afterwards.

## What the agent is

A **LaunchAgent that runs as you**, not a system daemon. It indexes your
files, so it needs your privacy grants and the shares mounted in your session;
a root daemon has neither. It starts when you log in, stops when you log out,
and keeps running while the screen is locked.

`~/Library/LaunchAgents/com.jnel.eidos.agent.plist` is the registration.

## Controlling it

```bash
eidos service status         # registration, load state, pid, API health
eidos service stop           # unload; the registration stays
eidos service start          # load, and wait until the API answers
eidos service restart
eidos service uninstall      # unload and remove the registration; data is kept
```

`stop` unloads the job rather than killing the process, because launchd would
restart anything that merely died. Re-running `install --replace` on a running
agent replaces its configuration and leaves it running: an install is a
configuration change, not an outage. `--timeout` bounds the whole command,
including a restart's stop and start together. Indexed data lives in the data directory
you chose and is never touched by `uninstall`.

Logs are in `<data-dir>/logs`: `eidos.log.<date>` is the rolling agent log,
and `launchd.out.log` / `launchd.err.log` capture anything that never reached
the logger, such as a crash during start-up.

## Signing

`build-agent.sh` signs with the first *Developer ID Application* identity in
your keychain, and falls back to an ad-hoc signature — valid on the machine
that produced it, not distributable — when there is none or when the keychain
cannot sign non-interactively. Notarised builds are produced by a release run
that holds the Apple credentials; see [releasing.md](releasing.md).

## What is not here yet

- A `.pkg` installer and a machine-wide (root daemon) installation.
- Anything that indexes files another account owns.
- A menu-bar interface: the agent has no window, only the local web interface.

## Troubleshooting

**A source shows zero files under my home directory.** The agent has no Full
Disk Access. Grant it to `Eidos.app` and restart the agent.

**`service start` reports the agent started but did not answer.** Read
`<data-dir>/logs/eidos.log.<date>`. The usual causes are a port already in use
and a data directory the agent cannot write.

**The source stays on periodic reconciliation instead of going live.** Its
volume keeps no FSEvents history — read-only volumes and some external ones do
not — so there is no cursor to resume. `eidos source list` shows the reason.
