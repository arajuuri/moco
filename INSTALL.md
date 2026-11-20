# Installing moco

moco is a single program file (`moco`) with no installer and nothing else to set up. Installing it means getting that file onto your Mac and putting it in a folder that is on your `PATH`, such as `/usr/local/bin`.

## Requirements

- An **Intel (x86_64) Mac** running **macOS 10.7 (Lion) through 10.14 (Mojave)**. Versions 10.8 to 10.13 should work but are untested. Newer macOS versions and Apple Silicon Macs are untested.
- A terminal (Terminal.app is fine). moco also works over SSH.
- An API key for an AI provider, for example from Anthropic, OpenAI or Google. moco itself is free, but the providers charge for their models.

## 1. Download

Open the [latest release](https://github.com/arajuuri/moco/releases/latest) and download two files:

- `moco-VERSION-macos-x86_64.tar.gz`, the program
- `moco-VERSION-macos-x86_64.tar.gz.sha256`, its checksum

`VERSION` is the release number, such as `0.2.2`. Keep both files in the same folder, for example `~/Downloads`.

**On macOS 10.7 (Lion) download on another computer.** Lion's `curl` cannot make the secure (TLS 1.2) connection GitHub requires, and its browser is too old to rely on. Download the two files on a newer computer and copy them over with a USB stick, a shared folder or `scp`:

```
scp moco-0.2.2-macos-x86_64.tar.gz* you@lion-mac.local:Downloads/
```

On newer systems you can also download from the terminal:

```
cd ~/Downloads
VERSION=0.2.2
curl -LO https://github.com/arajuuri/moco/releases/download/v$VERSION/moco-$VERSION-macos-x86_64.tar.gz
curl -LO https://github.com/arajuuri/moco/releases/download/v$VERSION/moco-$VERSION-macos-x86_64.tar.gz.sha256
```

## 2. Check the download

This confirms the file arrived intact:

```
cd ~/Downloads
shasum -a 256 -c moco-0.2.2-macos-x86_64.tar.gz.sha256
```

It should print `moco-0.2.2-macos-x86_64.tar.gz: OK`. If it says `FAILED`, download the file again. (The checksum comes from the same place as the file, so it catches damaged downloads, not a tampered release.)

## 3. Unpack

```
tar xzf moco-0.2.2-macos-x86_64.tar.gz
```

This creates a folder `moco-0.2.2-macos-x86_64` containing `moco` (the program), plus the license notices.

## 4. Put it on your PATH

**Option A: for everyone on the Mac (`/usr/local/bin`)**

```
sudo mkdir -p /usr/local/bin
sudo cp moco-0.2.2-macos-x86_64/moco /usr/local/bin/moco
sudo chmod 755 /usr/local/bin/moco
```

`sudo` asks for your account password. `/usr/local/bin` is on the default `PATH`, so nothing else is needed.

**Option B: for your user only (`~/.local/bin`), no password**

```
mkdir -p ~/.local/bin
cp moco-0.2.2-macos-x86_64/moco ~/.local/bin/moco
chmod 755 ~/.local/bin/moco
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bash_profile
```

Use `~/.zprofile` instead of `~/.bash_profile` if your shell is zsh (`echo $SHELL` shows it). Then open a new Terminal window.

## 5. If macOS refuses to run it

moco is not signed or notarized by Apple. A file you downloaded with a browser can be marked "quarantined", and macOS may then say it "cannot be opened because the developer cannot be verified" or ask you to confirm. Remove the mark once:

```
xattr -d com.apple.quarantine /usr/local/bin/moco
```

(Use the path where you put it. It is harmless if the mark was not there.) Files fetched with `curl` or `scp` are not marked.

## 6. Check that it works

```
moco --version
```

This prints the version, for example `moco 0.2.2`, and the macOS version it was built for. If the shell says `command not found`, the folder is not on your `PATH`: open a new Terminal window, check `echo $PATH`, and see step 4.

## 7. First run

```
moco auth anthropic        # paste your API key (it is not shown as you type)
cd ~/my-project
moco                       # starts the chat
```

Other providers: run `moco models` to see them and the environment variable each one reads. See the [usage guide](docs/11-usage.md) for configuration, permissions and commands. moco runs commands and edits files on your behalf, so read the disclaimer and the Safety section in the [README](README.md) first.

## Updating

Download the new release and repeat steps 1 to 4. Your sessions and API keys are kept.

## Uninstalling

```
sudo rm /usr/local/bin/moco             # or: rm ~/.local/bin/moco
```

Your sessions, logs and API keys live in `~/Library/Application Support/moco`. Delete that folder too if you want everything gone.

## Building from source

On a Mac with the Xcode command line tools (`xcode-select --install`), running `scripts/install.sh --add-to-path` from a checkout builds moco and installs it to `~/.local/bin`. Building needs Mojave or a newer release; on Lion, build on another Mac and copy the binary over. See [docs/11-usage.md](docs/11-usage.md) and [docs/14-macos-10.7.md](docs/14-macos-10.7.md).
