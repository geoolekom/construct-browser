# Construct Browser

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

**Construct Browser is a desktop app that runs your personal Construct on your own computer, under your control.**

It is part of [Constructor Fabric](http://www.constructorfabric.com/). The Construct gear of
[Constructor Fabric Gears](https://github.com/constructorfabric/gears-rust) builds a living profile of a
person from the tools they already use. Construct Browser is where that gear runs for one person: an
Electron window you open and close, and a Rust agent that keeps working in the background. The agent, the
data it collects and the settings stay on your machine. The only service the agent calls is the model you
configure yourself.

> **Status:** early development.

## Download

When a Linux x86_64 `.deb` or an Apple Silicon `.dmg` is available, download it from [GitHub Releases](https://github.com/constructorfabric/construct/releases).
A fork can publish its own releases by listing itself in the `RELEASE_REPOSITORIES` Actions variable
(see [mobile/RELEASING.md](mobile/RELEASING.md)).
`SHA256SUMS` covers the packages included when the release was first published. A package added later may
lack an entry. Check that the file lists your package before using it to verify the download.
Open a terminal in the download directory and install the package.

```bash
sudo apt install ./construct-browser_*.deb
```

For Apple Silicon Macs, get the `.dmg` from GitHub Releases, or the `macos-arm64-dmg` artifact from the
[Desktop release workflow](https://github.com/constructorfabric/construct/actions/workflows/release.yml).
Choose a successful run for a release tag or a reviewed commit on `main`. Check the run's ref and commit
before downloading, then extract the ZIP and follow the [macOS installation steps](#macos-apple-silicon).

For Windows x64, get `construct-browser-x64-Setup.exe` from GitHub Releases when available.
The same file is in the `windows-x64-setup` artifact from a successful Desktop release workflow
for a release tag or a reviewed commit on `main`. SmartScreen may warn about the unsigned installer.
Open Status in the app to see why the Construct agent is unavailable on Windows.

## What it does today

- **Reads your AI tools.** The agent follows the session logs of Claude Code and Codex, and the chats
  and Cowork sessions the Claude app keeps on a Mac, and turns every prompt and answer into a record,
  within seconds. Their files are only opened for reading; the Claude app's cache is read from a copy
  that is removed after each read.
- **Keeps the records on your computer.** The agent stores the records as plain files in its data directory.
- **Runs the Construct gear.** On Linux x86_64 and on macOS (Apple Silicon) the agent starts the Construct gear inside itself, next to an
  embedded PostgreSQL database with pgvector that serves graph storage. Nothing listens beyond your machine:
  the agent socket and the database socket are open to your user only, and the gear's REST interface is on the
  loopback address and needs a token that only your user can read.
- **Builds your profile from what you type.** Every new prompt in Claude Code, Codex or the Claude app goes to the
  Construct gear with the ten turns before it as context, and the gear turns it into facts about you.
  Prompts from before the first start go out only when you ask for them (see
  [Your profile from past prompts](#your-profile-from-past-prompts)).
- **Keeps working after you close the window.** The installed app registers the agent to start at login, as a
  systemd user service on Linux and a LaunchAgent on macOS.
- **Shows what the agent is doing.** The Status page shows whether the agent is running, its version, its
  process id, the model in use (`not set` until the address, the name and the key are all saved, and again
  after the key is removed) and whether it starts at login. It also shows the database and the Construct gear,
  each `ready`, `starting` or `failed`, with a short reason when it is not ready (for example
  `pgvector is not built for this platform yet` on macOS). While the gear is not ready, the Chat page says that
  the profile is not available, so an answer without facts about you is not taken for a fault of the model.
- **Talks to the model you choose.** The Settings page takes the address, the name and the API key of a model,
  can test the call, and can remove the key. The key is never written to a log or shown in the status. A
  change applies to the next call, with no restart.
- **Chat.** The Chat page streams the model's answers as Markdown, lets you stop an answer, and keeps your
  conversations on your computer.

The window follows the light or dark theme of your system.

## Your profile from past prompts

The agent sends every prompt you type after its first start to the Construct gear by itself. The history it
found in the logs of the last 30 days is not sent by itself, because every prompt costs one model call.
`backfill` sends it when you ask, oldest first, headless sessions included:

```bash
construct-agent backfill --dry-run     # count the prompts per tool, send nothing
construct-agent backfill --limit 50    # a small first run: the 50 oldest prompts
construct-agent backfill               # everything that is left (--days N changes the 30)
construct-agent status                 # the "facts" part shows the progress
```

The agent remembers which prompts it sent, so a prompt is never sent twice: not by a second `backfill`, not
after a restart or a crash, and not when a prompt is both in the history and new. A prompt goes out with the
turns before it in the same session, cut to the limits of the gear (4,000 characters for the prompt, 16,000 for
the context, the oldest turns first). Prompts go one at a time, and a new prompt goes before the rest of a
`backfill`. A `backfill` stops with its reason in `status` when the gear refuses it or the model does not answer.
Prompt text is never written to a log or to the status.

## The phone endpoint

The agent can listen for phones on your local network (`agent/src/phone`). A phone pairs once with a one-time secret, shown as a QR payload and as an 8-digit
code; after that every byte between the phone and the agent is encrypted, and the agent answers only paired phones.

- **Channel.** TLS 1.3 with one self-signed certificate that the agent makes on its first start and keeps in
  its config directory. The phone trusts that certificate only by the SHA-256 fingerprint in the QR payload,
  not by the address or a name, so a hotspot address or `10.0.2.2` of an Android emulator works. There is no
  plain HTTP, not even for the pairing.

- **Pairing.** The secret is never sent: the phone sends a keyed hash of the certificate it saw, the agent
  answers with a keyed hash of its own, and the agent hands the phone a token. The agent keeps only a hash of
  the token. A secret works once, for five minutes, and three wrong tries close the pairing. The wrong tries of all peers
  count together, so a peer on the network can stop a pairing; the person then opens a new one.

- **Strangers.** A connection that does not prove itself gets no answer: after the TLS handshake, which any peer can make,
  the agent sends nothing and closes it. Only a phone
  that proved itself is told that its protocol version is too old (`update needed`).

- **When it listens.** The agent opens no port before the first pairing. It listens while a pairing is open
  and, once a phone has paired, always (also after a restart) on port 47821, until the last phone is unpaired.
  The app asks through the agent's local protocol: `phone-pairing-open` (answers the QR payload and the
  code), `phone-pairing-status` (whether the offer is still usable), and `phone-pairing-close`.
  `phone-list {"after": null}` gives the first page of phones. Use the returned `next` as `after`
  until it is null. Each row gives an id, a name and the time of pairing. `phone-unpair {"id": "<id>", "ack_only": true}` ends that phone's open channels at once and acknowledges the removal.

To start the endpoint alone, with no database and no Construct host, and get an address and a code to pair
with:

```bash
cargo run -p construct-agent --example phone_pairing -- --host 127.0.0.1
```

It prints `payload` (the text of the QR code, `construct-pair://<address>?v=1&fp=<hex>&s=<hex>`), `address`,
`code`, `fingerprint` and `dir`, then `ready`, and keeps running until it is stopped. `--host` is the address
to put in the payload (the address of this computer's default route by default, which is the hotspot's on a
phone's hotspot; with no default route, or under a VPN that takes all traffic, name it), `--bind` the address to listen on
(`0.0.0.0:0` by default), `--dir` where the certificate and the paired phones are kept (a new temporary
directory by default) and `--minutes` how long the pairing stays open (5 by default). The tests are
`cargo test -p construct-agent --test phone`.

- **What a paired phone copies.** The facts, the chats and the dashboard numbers, with a token for each that
  changes when the part changes. The commands and the format of the items are described in the header of
  `agent/src/phone/sync.rs`.
- **When everything is deleted.** `sync-check` also answers `reset`, a text kept with the paired phones that changes
  each time `memory-erase` runs. A phone that sees a `reset` other than the one it saw last empties itself and keeps the
  new one; a phone that has none yet keeps the current one and empties nothing. While an erasure runs every sync
  command answers the error `erasing`, and the phone asks again later.

To serve made-up facts, chats and numbers instead of the real ones, give the example a file with `--data FILE`
(its shape is in the header of `agent/examples/phone_pairing.rs`). The example reads the file again at every
command, so a test can change it while the program runs. Stop it with Ctrl-C (or `stop` on stdin with `--control`)
and start it again with the same `--dir` and a fixed port in `--bind`, for example `--bind 0.0.0.0:47821`: it keeps its
certificate and its paired phones, so a paired phone connects again to the same address.

## Size and memory

Measured on Linux x86_64 with the release build (`make build`): the agent is one 118 MB file, with the
database inside it. With the database and the gear ready and nothing to do, the agent holds about 57 MB and
PostgreSQL about 35 MB (proportional set size, so shared memory counts once), 92 MB together. The first start,
which creates the database, takes about 100 MB for the agent.

## Where your data lives and who can read it

The records, the profile and the database are plain files under your user's data directory
(`~/.local/share/construct` on Linux), readable by your user only. The agent does not encrypt them: it relies on
the disk encryption of your operating system (FileVault on macOS, BitLocker on Windows, LUKS on Linux). Turn it
on if the computer can be lost or shared.

## What works on each system

| Part                                                         | Linux x86_64         | macOS (Apple Silicon)    | Windows                                                                |
| ------------------------------------------------------------ | -------------------- | ------------------------ | ---------------------------------------------------------------------- |
| Window, Status, Settings and Chat pages                      | yes                  | yes                      | the window opens; Status says `unknown`, Settings says "not available" |
| Agent socket and `status`                                    | yes                  | yes                      | no: the agent stops with "not implemented on this platform"            |
| Start at login                                               | systemd user service | LaunchAgent              | no: "not implemented on Windows yet"                                   |
| Log readers (Claude Code, Codex), settings, model test, chat | yes                  | yes (no database needed) | no: the agent does not start                                           |
| Embedded PostgreSQL with pgvector                            | yes                  | yes                      | no                                                                     |
| Construct gear                                               | yes                  | yes                      | no                                                                     |

`make smoke` installs the Linux package in a clean Ubuntu 24.04 container and starts it there. Tested on Linux x86_64 (Ubuntu 24.04).

The macOS cells for the database and the Construct gear rest on a manual run on an Apple Silicon Mac (macOS 27.2): the packaged agent reported the database and the gear `ready`, and `create extension vector` with the `<->` operator worked. The integration test that starts PostgreSQL (`agent/tests/database.rs`) runs on Linux only. On macOS the agent does not yet recognise a PostgreSQL left running by an agent that was killed outside launchd (it looks the process up in `/proc`), so that case is untested there.

## Build and test

You need Rust (the toolchain in `rust-toolchain.toml` is installed by `rustup`), Node.js 22.13 or newer (22 or 24) with npm, and
`protoc` with the protobuf headers (Debian and Ubuntu: `protobuf-compiler` and `libprotobuf-dev`; macOS:
`brew install protobuf`). The agent build (`make build`, and so `make package`) also builds pgvector against the
bundled PostgreSQL; for that it needs `curl` and `tar`, and on Linux `make` and `gcc`, on macOS the Xcode command
line tools for `make` and `clang` (`xcode-select --install`). `make test` needs `xvfb-run` for the Electron window, so it runs on Linux. `make smoke` and the Linux package (`npm run make`) also need `dpkg` and `fakeroot`.

```bash
make build    # build the agent: target/release/construct-agent
make test     # format check, clippy, agent tests, then the app tests and the Electron test
make lint     # the app's lint and type check
make run      # build the agent and start the app in development mode
make package  # package the app with the agent inside
make smoke    # build the .deb, install it in a clean Ubuntu 24.04 container and start it (needs Docker)
```

`make help` lists the targets. `make test` runs the Electron test under a virtual display; it opens the
window, takes a screenshot of each page in both themes and fails on any content security policy violation.
`make smoke` checks that the installed agent answers `status` with `running` and that the app is still alive
after 15 seconds.

## Install from source

Build the app from source when a [download](#download) is unavailable for your system or you need a specific
revision. First install the tools from [Build and test](#build-and-test), then follow the steps for your system.
All commands run in the root of this repository.

### Linux (Ubuntu and Debian, x86_64)

```bash
make build app-deps
(cd app && npm run make)
sudo apt install ./app/out/make/deb/x64/*.deb
```

Start **Construct Browser** from your application menu, or run `construct-browser`. The installed app
registers the agent to start at login. `make smoke` builds the package and installs it in a clean container.

### macOS (Apple Silicon)

To build the disk image locally, run the following commands.

```bash
make build app-deps
(cd app && npm run make)
```

Electron Forge writes the disk image to `app/out/make/dmg/arm64/`. A disk image built this way, or
by a pull request, uses an ad hoc signature and ships without notarization. The disk image of a release
(a `v*` tag, or a manual run of the Desktop release workflow) is signed with the team's Developer ID
certificate and notarized in the `release` environment, and it opens without the
Gatekeeper exception below.

1. Open the disk image and drag **Construct Browser** into **Applications**. For an update, quit the
   running app first and choose **Replace** when prompted.
1. Open the app from **Applications**. Use the Gatekeeper exception only for a disk image built from a
   reviewed commit or release tag. Go to **System Settings > Privacy & Security** and click
   **Open Anyway**. You can also clear quarantine for that copy with
   `xattr -dr com.apple.quarantine "/Applications/Construct Browser.app"`.
1. On first launch, the app registers the agent in `~/Library/LaunchAgents/cf.construct.agent.plist`.
   The Status page shows `running` once the agent starts.

The Apple Silicon agent bundles pgvector. If pgvector is unavailable in a build, the database on the
Status page shows `failed` with the reason `pgvector is not built for this platform yet`.

The app declares why it sends Apple Events (`NSAppleEventsUsageDescription` in its `Info.plist`), which macOS needs
before it asks you to let the agent read the tab of your browser.

To grant Accessibility access, open **System Settings > Privacy & Security > Accessibility**, click **+**,
choose `/Applications/Construct Browser.app/Contents/Resources/construct-agent` and turn it on.
If macOS stops recognizing that grant after an update, remove the old entry and add the new agent.

### Windows

To start the window in development mode, install Node.js and run in PowerShell:

```powershell
cd app
npm ci
node node_modules/electron/install.js
npm start
```

## Contributing and license

Contribution rules and how to report a security issue are in the
[Constructor Fabric governance repository](https://github.com/constructorfabric/governance), starting with
[CONTRIBUTING.md](https://github.com/constructorfabric/governance/blob/main/CONTRIBUTING.md). Commits are
signed off, as the [DCO](https://github.com/constructorfabric/governance/blob/main/DCO.md) asks.

Construct Browser is licensed under the Apache License 2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
