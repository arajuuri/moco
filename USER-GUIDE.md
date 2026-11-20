# moco user manual

This manual is for people who are not programmers. It takes you from nothing to your first useful conversation with moco. You do not need to know how to code, but you will type a few short commands into the Terminal, and each one is spelled out below.

> **Read this first.** moco was written by AI and has not been reviewed by humans (see the disclaimer in the [README](README.md)). It can create, change and delete files on your Mac, and it can run commands. Use it on copies of your files, or in a folder you have a backup of, until you trust it.

## 1. What moco is

moco is an AI assistant that lives in the Terminal, the Mac's text-based window. You write what you want in plain language, for example "list the files in this folder and tell me which are the largest", and moco works on it: it reads files, makes changes, runs commands and tells you what it did. It is made for older Intel Macs (macOS 10.7 Lion through 10.14 Mojave) that newer AI tools no longer support.

moco is only the program. The "thinking" is done by an AI service you choose (Anthropic, OpenAI or Google). Your messages and the parts of your files moco reads are sent to that service. That service **charges you for use**, and moco itself is free.

## 2. What you need

- An **Intel Mac** with **macOS 10.7 to 10.14**. (Newer systems and Apple Silicon Macs have not been tested.)
- An **internet connection**.
- An **account with an AI provider and an API key** (section 3). A chat subscription such as a ChatGPT or Claude plan is not the same thing and will not work as a key.
- About ten minutes.

## 3. Get an API key

An API key is a long password that lets moco use your provider account. Pick one provider:

| Provider | Where to sign up |
|---|---|
| Anthropic (Claude), moco's default | console.anthropic.com |
| OpenAI | platform.openai.com |
| Google (Gemini) | aistudio.google.com |

Sign up, add a payment method or credit if asked, and create an API key. Copy it somewhere safe for the next few minutes. Treat it like a password: don't share it or post it anywhere.

**Set a spending limit** in your provider's account settings if it offers one. An assistant that works on its own can use a surprising amount of paid AI time, and a limit keeps the bill predictable. moco shows the cost of each reply, which is covered in section 7.

## 4. Install moco

### 4.1 Open the Terminal

Press **Command-Space**, type `Terminal` and press Enter. A window with a blinking cursor opens. This is where you type the commands in this manual. Type each one exactly, then press **Enter**.

### 4.2 Download

On a newer Mac or on macOS 10.8 and up, open the [latest release page](https://github.com/arajuuri/moco/releases/latest) and download these two files into your **Downloads** folder:

- `moco-VERSION-macos-x86_64.tar.gz` (the program)
- `moco-VERSION-macos-x86_64.tar.gz.sha256` (a check that the download is not damaged)

`VERSION` is a number such as `0.2.2`. In the commands below, replace `0.2.2` with the number you downloaded.

**If your Mac runs Lion (10.7):** it cannot download from GitHub. Download the two files on another computer and copy them over with a USB stick.

### 4.3 Check, unpack and install

Type these lines one at a time. The first one moves into your Downloads folder.

```
cd ~/Downloads
shasum -a 256 -c moco-0.2.2-macos-x86_64.tar.gz.sha256
```

You should see `moco-0.2.2-macos-x86_64.tar.gz: OK`. If you see `FAILED`, download the file again.

```
tar xzf moco-0.2.2-macos-x86_64.tar.gz
sudo mkdir -p /usr/local/bin
sudo cp moco-0.2.2-macos-x86_64/moco /usr/local/bin/moco
sudo chmod 755 /usr/local/bin/moco
```

`sudo` asks for your Mac's password. Nothing appears as you type it. That is normal. Type it and press Enter.

### 4.4 If macOS refuses to open it

moco is not signed by Apple, so a file downloaded with a web browser may be blocked with a message such as "cannot be opened because the developer cannot be verified". Run this once to lift the block:

```
sudo xattr -d com.apple.quarantine /usr/local/bin/moco
```

If it says there is no such attribute, nothing was blocking it, which is fine.

### 4.5 Check that it works

```
moco --version
```

You should see a line starting with `moco` and the version number. If you see `command not found`, close the Terminal window, open a new one and try again. If it still fails, repeat step 4.3.

## 5. Connect moco to your AI provider

Run the line for your provider. moco asks you to paste your API key. **Nothing appears on screen when you paste. That is normal.** Press Enter.

```
moco auth anthropic
```

or `moco auth openai`, or `moco auth google`.

The key is stored in a private file that only your Mac account can read.

- **Anthropic:** nothing more to do.
- **OpenAI or Google:** moco starts with an Anthropic model, so you must name a model once. Run `moco models openai` (or `moco models google`) to see the list, then start moco with `moco --model openai/MODEL` using a name from that list, for example `google/gemini-3.8-flash`. Section 9 shows how to make that permanent.

## 6. Your first session

moco works inside a folder, and it can see and change the files in that folder. Choose a folder deliberately. For a first try, create a practice folder:

```
mkdir ~/moco-practice
cd ~/moco-practice
moco
```

The screen fills with the moco chat. At the bottom is a box where you type.

Try:

> Create a file called hello.txt that says hello, then show me what is in this folder.

Press **Enter** to send. moco's answer appears as it is written. Lines starting with `>` show the steps it takes, such as writing a file.

### Permission requests

By default moco **asks before it changes anything**. When it wants to write a file, run a command or fetch a web page, the typing box is replaced by a question that shows exactly what it wants to do. Reading and searching do not ask. Your answers:

| Key | Meaning |
|---|---|
| `y` | Yes, this once |
| `a` | Yes, and always for this kind of action in this folder |
| `n` | No, and stop |
| `f` | No, and let me tell moco why |

Read the request before answering. If you don't understand what it will do, press `n` and ask moco to explain. Use `a` only for things you are happy to allow every time, such as running a harmless command you have seen several times.

## 7. Everyday use

**Say what you want, in your own words.** Be specific: "Rename every file in this folder so that it starts with the date" works better than "tidy up". You can ask moco to explain what it found, to continue, or to change its approach. Follow-up messages work like a normal conversation.

**Name files with `@`.** Typing `@report.pdf` or `@notes.txt` attaches that file to your message. Press **Tab** to complete a file name. Images (`@photo.png`) and PDFs work with models that can read them.

**Watch the cost.** After each reply moco prints a line with the cost of that reply and of the whole session. Long conversations and large files cost more.

**Stop it.** Press **Esc** (or **Control-C**) to interrupt moco while it is working.

**Undo.** Type `/undo` to take back the file changes from the last request. Type `/redo` to bring them back. `/diff` shows everything that changed in the folder since you started.

**Leave.** Press **Control-D**, or type `/exit`.

**Come back later.** In the same folder, run `moco --continue` to pick up where you stopped. `moco sessions` lists past conversations.

**Ask for help.** Type `/help` in the chat for the list of commands.

### Useful chat commands

| Type | What it does |
|---|---|
| `/undo` and `/redo` | Take back or restore the last set of file changes |
| `/diff` | Show what changed in the folder |
| `/model` | Show or change the AI model |
| `/compact` | Shorten a very long conversation so it costs less and keeps working |
| `/new` | Start a fresh conversation |
| `/exit` | Quit |

### One-off questions

For a single question without opening the chat, type it after `-p`:

```
moco -p "What is in this folder?"
```

moco answers and exits.

## 8. Staying safe

- **Work in a folder you have backed up** (Time Machine counts), or a copy of your files.
- **Leave moco on its default "manual" mode**, where it asks first. The "auto" mode (`/mode auto`, or Shift-Tab) lets it act without asking. Use it only in a practice folder.
- **Read each request** before pressing `y`, especially commands that delete (`rm`), move or overwrite files, or that connect to the internet.
- **Don't put secrets** such as passwords or banking details in the folder you are working in. moco may read files there and send their contents to your AI provider.
- **Keep your API key private.** If it leaks, delete it in your provider's account and create a new one.
- If something goes wrong, press **Esc**, then `/undo`.

## 9. Making your settings permanent

To use a model other than the default every time, create a settings file. Run:

```
mkdir -p ~/Library/Application\ Support/moco/config
open -e ~/Library/Application\ Support/moco/config/moco.json
```

TextEdit opens. Put this in the file (use a model name from `moco models`), save and close:

```
{ "model": "openai/MODEL" }
```

Replace `openai/MODEL` with your choice, for example `google/gemini-3.8-flash`.

## 10. Updating and removing moco

**Update:** download the new release and repeat step 4.3. Your conversations and key are kept.

**Remove:**

```
sudo rm /usr/local/bin/moco
```

Your conversations, settings and API key are kept in `~/Library/Application Support/moco`. Delete that folder as well if you want everything gone.

## 11. When something goes wrong

| What you see | What to do |
|---|---|
| `command not found: moco` | Close the Terminal, open a new window, try again. If it persists, repeat step 4.3. |
| macOS says the developer cannot be verified | Run the command in step 4.4. |
| `FAILED` in the checksum step | Download the file again. |
| moco says it has no API key or model access | Run `moco auth PROVIDER` again (step 5). For OpenAI or Google, add `--model`. |
| The answer stops or errors | Check your internet connection and your provider account's credit or limit, then try again. |
| moco seems stuck | Press **Esc**. If that does not help, **Control-C**, then start again with `moco --continue`. |
| Messy or garbled screen | Make the Terminal window bigger, or run `moco --plain` for a simpler display. |

`moco logs` prints recent diagnostic messages, which are useful when you report a problem at [github.com/arajuuri/moco/issues](https://github.com/arajuuri/moco/issues). Check them for private information first.

## 12. Where to learn more

- [INSTALL.md](INSTALL.md): the install steps in more detail, for people comfortable with the Terminal
- [docs/11-usage.md](docs/11-usage.md): the full reference of commands and settings
- [README.md](README.md): what moco is and its safety notes
