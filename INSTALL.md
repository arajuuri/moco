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

moco needs an API key from one AI provider. Pick one. Each command asks for the key and does not show it as you type. It is stored in `~/Library/Application Support/moco/auth.json`, readable only by you.

**Anthropic (Claude)** is the default, so nothing else is needed:

```
moco auth anthropic
cd ~/my-project
moco
```

**OpenAI:**

```
moco auth openai
moco models openai                  # list the models you can use
cd ~/my-project
moco --model openai/MODEL           # MODEL is a name from that list
```

**Google (Gemini):**

```
moco auth google
moco models google
cd ~/my-project
moco --model google/MODEL
```

`--model` is needed because moco starts with an Anthropic model unless you say otherwise. To avoid typing it every time, set `MOCO_MODEL` (for example `export MOCO_MODEL=google/MODEL` in your `~/.bash_profile`), or put the model in the config file `~/Library/Application Support/moco/config/moco.json`:

```
{ "model": "openai/MODEL" }
```

Instead of `moco auth`, you can set the key in the environment: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `GEMINI_API_KEY`. Other providers (OpenRouter, DeepSeek and more) work the same way: `moco auth PROVIDER`, then `--model PROVIDER/MODEL`. `moco models` shows every provider and whether it is connected. You can switch models inside the chat with `/model`.

See the [usage guide](docs/11-usage.md) for configuration, permissions and commands. moco runs commands and edits files on your behalf, so read the disclaimer and the Safety section in the [README](README.md) first.

## Updating

Download the new release and repeat steps 1 to 4. Your sessions and API keys are kept.

## Uninstalling

```
sudo rm /usr/local/bin/moco             # or: rm ~/.local/bin/moco
```

Your sessions, logs and API keys live in `~/Library/Application Support/moco`. Delete that folder too if you want everything gone.

## Building from source

On a Mac with the Xcode command line tools (`xcode-select --install`), running `scripts/install.sh --add-to-path` from a checkout builds moco and installs it to `~/.local/bin`. Building needs Mojave or a newer release; on Lion, build on another Mac and copy the binary over. See [docs/11-usage.md](docs/11-usage.md) and [docs/14-macos-10.7.md](docs/14-macos-10.7.md).
