# Notebooks

## Boot benchmark: `omnidroid_boot_bench.ipynb` (real Android, no account)

[Open in Colab](https://colab.research.google.com/github/devflaryn/omnidroid/blob/main/notebooks/omnidroid_boot_bench.ipynb)

Times how fast a machine's CPU and RAM boot omnidroid's real-AOSP path (Android 15 on omnidroid's
Linux ABI layer, `omni-linux`'s `r_roblox` session) and start the Roblox APK, headless. It builds
`main`, builds the Android system from Google's public emulator image (`arm64-v8a-35_r02.zip`), and
caches both in Drive. Then: one **new device** (first boot, `pm install`, the app's first start,
the device saved) and `SAVED_BOOTS` boots of the **saved device** (what every later session does),
each timed from the log (`boot_completed`, the app on screen) while CPU busy, memory and load are
sampled every second. **No cookie is used and nothing signs in.** Results go to Drive
`MyDrive/omnidroid/bench/*.json`; the last cell compares every runtime measured so far.

You bring only the APK: Drive `MyDrive/omnidroid/*.apk` (or `/content`, or `APK_PATH`).

## Headless play: omnidroid headless on Colab, Kaggle and Modal (the old path)

`omnidroid_headless.ipynb` builds omnidroid from source, starts Roblox with no display at all
(`omnidroid --no-window --control <file>`, in `unified` from `dce9ac0`), joins a
place (Pet Simulator 99, `8737899170`, by default), waits for the world to load and takes a
screenshot as proof -- shown in the notebook and printed as a `data:image/jpeg;base64,...` URL to
paste into a browser's address bar. The same notebook runs on Google Colab, Kaggle, a Modal
notebook, or any Ubuntu/Debian Jupyter; it detects which. `modal_app.py` is the Modal *function*
version: the build is baked into an image, and `modal run` brings the screenshot back to your
terminal.

You bring two things, neither of which is in the repository:

* **the APK** -- the **stock** `Roblox-2.738.1397.apk` (arm64, 229,466,269 bytes), sha256
  `bbe00ae306cc251c4ea55b7a932d9c524ecb0d6d9203c2a6161bcf0fae792742`. The notebook and the Modal
  app hash it and **refuse any other file**: a modified APK given a cookie can take the account.
  For another version, set `APK_SHA256` (`OMNI_APK_SHA256`) to that version's stock hash;
* **a cookie** -- the account's `.ROBLOSECURITY` value (a text file holding the bare value,
  `.ROBLOSECURITY=<value>` or a Netscape `cookies.txt` all work).

> **Read before using a cookie.** Roblox can end a session -- for good -- when its cookie arrives
> from another country than the one it is used from. Colab, Kaggle and Modal run in US/European
> data centres. A cookie made in Turkey, used there, is at risk. Use an account that lives in the
> region the notebook runs in (or a throwaway one). Set `ACCOUNT_COUNTRY` (notebook) or
> `OMNI_ACCOUNT_COUNTRY` (Modal app) to the cookie's country, e.g. `TR`, and the run **stops
> before using the cookie** when the machine's exit country differs. The cookie is kept in the
> platform's secret store and never printed.

## Google Colab

1. Open the notebook in Colab (File > Upload notebook, or open it from GitHub/Drive).
2. **APK:** put it in Google Drive at `MyDrive/omnidroid/` (any `*.apk` there is found). The
   notebook mounts Drive and asks you to allow it. Or drag it into the Files panel (`/content`),
   which is lost when the runtime ends.
3. **Cookie:** the key icon (Secrets) in the left bar > *Add new secret*: name `ROBLOSECURITY`,
   value the cookie; switch *Notebook access* on.
4. **Private repository only:** a secret `GITHUB_TOKEN` (a read-only fine-grained token).
5. **GPU:** Runtime > Change runtime type > T4 GPU (or CPU). See "GPU or CPU" below.
6. Runtime > Run all. The screenshot is in the output of cell *10. Screenshot*.

The build is cached in `MyDrive/omnidroid/cache/` (~0.1 GiB per commit); the next session
restores it instead of building. Free Colab disconnects idle sessions after ~90 min and ends any
after ~12 h.

## Kaggle

1. New Notebook > File > Import Notebook > this `.ipynb`.
2. **Settings > Internet: On** (phone-verified accounts only). Without it nothing installs.
3. **APK:** make a *private* dataset holding the `.apk` (Datasets > New Dataset), then in the
   notebook *Add Input* > that dataset. It appears under `/kaggle/input/<name>/`.
4. **Cookie:** Add-ons > Secrets > Add a new secret `ROBLOSECURITY`, and tick it for this notebook.
   (`GITHUB_TOKEN` likewise for a private repository.)
5. **Accelerator:** Settings > Accelerator: GPU T4 x2 / P100, or None.
6. Run All. Screenshots are also saved in `/kaggle/working/` (the notebook's output).

**Cache:** after the first full build the notebook writes
`/kaggle/working/omnidroid-build-<commit>-rust<version>-ubuntu22.04.tar.zst`. *Save Version*, then
from the version's Output make a dataset of that file and *Add Input* it next time: the notebook
finds it under `/kaggle/input/` and skips the build.

## Modal

**As a notebook** (modal.com > Notebooks): upload the `.ipynb`; attach a Volume holding the APK
(the Files panel; it appears under `/mnt/<volume>/`) and a Secret holding `ROBLOSECURITY` (it
becomes an environment variable); pick a GPU or none in the kernel settings; run all. The build
cache is written to the Volume (`/mnt/<volume>/cache/`). Raise the idle shutdown (default 10 min)
if you want the game to keep running between cells.

**As a function** (`modal_app.py`), from your own terminal:

```sh
pip install modal && modal setup
modal volume create omnidroid
modal volume put omnidroid Roblox-2.738.1397.apk /Roblox-2.738.1397.apk
modal secret create roblox-cookie ROBLOSECURITY="$(cat my-cookie.txt)"
modal run notebooks/modal_app.py                     # CPU
OMNI_MODAL_GPU=T4 modal run notebooks/modal_app.py   # with a GPU
```

The first run builds the image (Ubuntu 22.04, its packages, Rust, the release build) on an
8-CPU builder: ~10-15 min, once per commit. Each run then takes ~5-10 min and writes
`omnidroid-shot.png` / `.jpg` beside you and prints the `data:` URL. The app's storage is kept on the Volume
(`data/<account>`), so a cookie Roblox rotates is kept for the next run. Other settings
(`OMNI_BRANCH`, `OMNI_MODAL_REGION`, `OMNI_ACCOUNT_COUNTRY`, ...) are listed at the top of the
file.

## Your own Linux machine

Ubuntu or Debian with Jupyter. With root or passwordless `sudo` the notebook installs its
packages; otherwise it checks them and names what is missing:

```sh
sudo apt-get install -y build-essential cmake ninja-build pkg-config git curl zstd \
  libasound2-dev libx11-dev libxi-dev libxfixes-dev libvulkan1 libvulkan-dev mesa-vulkan-drivers \
  libegl1 libgles2 libegl-mesa0 libgl1-mesa-dri libglvnd0
```

Set `APK_PATH`, and the cookie as the `ROBLOSECURITY` environment variable or `COOKIE_FILE`.
Every parameter can also come from the environment as `OMNI_NB_<NAME>`, so it also runs
unattended:

```sh
OMNI_NB_APK_PATH=~/Roblox.apk OMNI_NB_COOKIE_FILE=~/cookie.txt OMNI_NB_RENDERER=cpu \
  jupyter nbconvert --to notebook --execute --ExecutePreprocessor.timeout=-1 omnidroid_headless.ipynb
```

Without root, the source and build go to `~/omnidroid-nb/src` and the run to `~/omnidroid-nb/run`.

## GPU or CPU

* There is **no display**: `--no-window` gives the engine an off-screen EGL pbuffer (NVIDIA's EGL
  device, or Mesa's surfaceless platform) and it renders with GLES. It starts **headless** -- the
  game runs and presents frames, nothing is drawn -- unless `START_HEADLESS` is off; `headless
  off` / `headless on` switch drawing live, and `screenshot <path>` renders that one frame for
  real either way.
* **GPU:** with an NVIDIA GPU whose EGL library the platform exposes (Colab ships it in
  `/usr/lib64-nvidia`; Kaggle's and Modal's containers may not -- the notebook looks, and says),
  the notebook writes the glvnd vendor file for `libEGL_nvidia.so.0` and the game renders on the
  GPU. `RENDERER` forces `gpu` or `cpu`.
* **CPU:** Mesa **llvmpipe** (`LIBGL_ALWAYS_SOFTWARE=1`). Measured on 4 old cores: the place loads
  and the world is drawn at ~5 frames/s, ~25 while headless.
* Vulkan is limited to lavapipe, which the engine refuses by its own rule, so it always uses its
  GLES renderer (the path measured on Linux).
* There is no sound card on these machines. ALSA says `Unknown PCM default` and the game runs on
  without sound.

## Timings

| step | measured on a 4-core i5-4460 (2014), CPU only, shared with other jobs |
|---|---|
| packages (stock Ubuntu 22.04) | 1:40 |
| Rust toolchain (rustup, minimal) | 0:24 |
| first build | 5:15 on Ubuntu 22.04 (g++ 12), 6:18 on 26.04; expect ~10-15 min on 2 vCPUs |
| build cache | 0.11 GiB; restored and fresh in ~2 s |
| start to signed in | 0:22 |
| start to "the place has loaded" | 1:48 |
| start to the world on screen | 2:48 (the game's own loading screen in between) |
| llvmpipe frame rate in PS99, 1280x720, lowest quality | 5-9 frames/s on 4 cores |

A run holds ~2.5-4 GiB of memory (2.3 GiB peak resident measured). 2-vCPU machines (Colab
free) work, more slowly: llvmpipe shares those two CPUs with the game.

## When it goes wrong

* **`branch 'unified' not found`**: the branch is not on GitHub yet (or the repository is private:
  add `GITHUB_TOKEN`). Set `BRANCH`, or upload a source tarball and set `SOURCE_TARBALL`.
* **"has no --no-window/--control"**: the branch predates headless mode (`dce9ac0`). Use `unified`
  from that commit on.
* **"not signed in after 3 minutes"**: the cookie was rejected -- expired, or ended by Roblox
  (see the country warning). Export a fresh one.
* **The game exits before the place loads**: the last 40 log lines are printed; the whole log is
  `game.log` in the run directory. `GUEST THREAD DIED` lines are runtime bugs worth reporting.
* **"REFUSED: ... has sha256 ..."**: the APK is not the stock build. Use the Play Store's.
* **"still a loading screen"**: after the place loads, the game shows its own loading screen until
  the world has streamed in; the wait cell looks at the screen every 20 s and goes on once it is
  no longer one flat colour. On a busy 2-CPU machine that can take several minutes; raise
  `LOAD_TIMEOUT_MINUTES`, or take another screenshot with cell 12 later.
* **Stopping**: the last cell interrupts the game; a session that reaches `SESSION_MINUTES`
  closes cleanly instead. The engine judges an interrupted session a crash at the next start in
  the same storage (`ACCOUNT_NAME`'s); if a later start misbehaves, use a new `ACCOUNT_NAME` (a
  fresh storage directory -- the cookie is planted again).
