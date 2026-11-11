<p align="center"><img src="assets/logo.svg" alt="apkscope logo" width="112"></p>

<h1 align="center">apkscope</h1>

<p align="center">
  <b>Open any Android app and see what it really does.</b><br>
  What it collects, how it makes money from you, what it exposes. In seconds, as readable Java.
</p>

<p align="center">
  <a href="#get-started"><img src="https://img.shields.io/badge/Get_started-30_seconds-f0883e?style=for-the-badge" alt="Get started"></a>
  <a href="#what-it-found"><img src="https://img.shields.io/badge/Real_findings-5_apps-db2777?style=for-the-badge" alt="Real findings in 5 apps"></a>
  <a href="BENCHMARKS.md"><img src="https://img.shields.io/badge/Benchmarks-10_popular_apps-2ea44f?style=for-the-badge" alt="Benchmarks: 10 popular apps"></a>
  <a href="https://github.com/suffixing/apkscope/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/suffixing/apkscope/ci.yml?branch=main&style=for-the-badge&label=CI" alt="CI"></a>
</p>

<p align="center">
  <a href="#what-it-found">Findings</a> ·
  <a href="#speed">Speed</a> ·
  <a href="#get-started">Install</a> ·
  <a href="#commands">Commands</a> ·
  <a href="#built-for-ai-agents">MCP</a> ·
  <a href="#the-viewer">Viewer</a> ·
  <a href="#honest-limits">Limits</a>
</p>

> **In 10 seconds:** one executable, no Java. Drop in an APK and get a security overview, readable
> Java, every endpoint and who calls it. Use it as a **viewer**, a **CLI**, or an **MCP server** your
> AI agent drives. Opens TikTok **11x faster than JADX with 9x less RAM**, and decompiles a class
> **up to 1.8x faster than rust_ASC**, as typed Java instead of pseudocode.

## What it found

<p align="center"><img src="docs/findings.svg" alt="Findings ranked: 1 Block Blast prices you for advertisers; 2 Block Blast's server can switch on an audit mode; 3 Nextcloud lets any app read your file index; 4 AntennaPod trusts user certificates; 5 Thunderbird ships a debug activity, safe on release; 6 KeePassDX's ECB flag is correct" width="760"></p>

Each one needed the manifest, the resources and the decompiled code read together. A grep for
`http://` finds none of it. Click a finding for the proof: real output, trimmed to the lines that matter.

<details>
<summary><b>1. Block Blast prices you for advertisers</b> · 13 ad networks, and its own model of what you're worth</summary>

<br>

Block Blast! 10.9.3 (Play build), 75,938 classes, scanned in 3.1 s.

**Who gets your data:** AppLovin MAX, AdMob, Meta, Unity, ironSource, Pangle, Liftoff/Vungle, InMobi,
Moloco, Amazon APS, Digital Turbine/Fyber, PubMatic, Bigo. Trackers: AppsFlyer, Firebase, Facebook and
TikTok events, ThinkingData, the studio's own `cn.hsdata`. Permissions: advertising ID and all four
Privacy Sandbox ad APIs.

**Two endpoints aren't from any SDK.** `infer` is a model call: what are this player's ads worth?

```text
$ apkscope search blockblast.xapk urls
q.mb("https://low-ecpm.afafb.com/infer/v1/low_ecpm_cut", appActivity, jSONObject, new c() {
return "https://ecpm.afafb.com/infer/v1/ecpm";

$ apkscope decompile blockblast.xapk me.f          # the code calls it AlgorithmInterCutManager
    m2.mg(jSONObject, "ActUserInfo", ClassE.ma());
    m2.me(jSONObject, "AdHistInfo", jSONArray);
    m2.mg(jSONObject, "UserContext", ClassE.mc(str2, appActivity, z, UUID.randomUUID().toString(), "INTER"));
    m2.mg(jSONObject, "UserInfo", ClassE.md());
```

Those fill in `deviceModel`, `ram`, `disk`, `cpu`, `carrier`, `country`, `ip_country`, `timeZone`,
`install_time`, `cold_start_num` and your past ads with their `revenue`. The answer sets your floor
price, the lowest bid an advertiser must beat to show you an ad:

```text
$ apkscope decompile blockblast.xapk v2.d --method Y        # AlgorithmBy1004, AppLovin MAX
    double d3 = d * d2;     // floor = predicted eCPM x "floor_factor" (remote, default 2.0)
    this.g_c.mi(d3);
        String.format(Locale.locale_US, "算法接口成功更新底价 - FloorPrice: %.2f, ...   // "algorithm API updated the floor price"
```

**Verdict:** legal, and how ad-funded games work. Now you can see it.
</details>

<details>
<summary><b>2. Block Blast's server can switch the game into "audit mode"</b> · found in code the packer hid</summary>

<br>

Block Blast hides its game classes behind a NetEase packer: `org.cocos2dx.javascript.AppActivity` isn't
in the APK. `apkscope dump` let the game unpack itself and pulled 15 DEX out of memory in 25 s, about
3,100 hidden classes. The scripts call Java through `JsCallJava`; its methods show what they ask for:

```text
$ apkscope dump com.block.juggle out/ --attach
$ apkscope decompile out/dex/classes9.dex org.cocos2dx.javascript.pay.o --method w   # SPayManager.serverControl
    if (jSONObject.has("isStatus")) {
        boolean z = k0.ma("1", jSONObject.getString("isStatus"));
        SPStore.mk().y(z);
        JsCallJava.setGameAuditMode(z, "auditMode");      // → window.IPAUtils.respHearingState(1)
```

- **A server switch.** The payment manager's server reply turns `auditMode` on in the game's scripts,
  and the game remembers it. Games use this to act differently while a store reviews them; what
  Block Blast changes is in its encrypted scripts, so we don't claim more.
- **Your phone, on request:** `callNativeGetLeftBattery` (battery %), `callNativeIsInstallOtherApp`
  (is app X installed?), `getDeviceInfoByAdb` (CPU speeds, RAM, storage, screen, model).
- **Revenue per round:** `trackGameEndRevenue` stores revenue data at the end of each game.
- **Bluetooth is benign:** `BleBumpKit` pairs two phones nearby, after asking permission.

Not shown: how it decides your next blocks. That's in the encrypted scripts (see [limits](#honest-limits)).
</details>

<details>
<summary><b>3. Nextcloud lets any app on the phone read and edit your file index</b></summary>

<br>

The file-database provider is exported, and its `<path-permission>` tags name a permission literally
called `"false"`. The only real guard is in code, and it skips `file/`, `dir/` and the root:

```text
$ apkscope decompile nextcloud.apk com.owncloud.android.providers.FileContentProvider
    private boolean isCallerNotAllowed(Uri uri) {
        switch (i) {
            case 4: case 5: case 6: case 7: case 8:     // shares, capabilities, uploads, ...
                str = this.mContext.getPackageManager().getNameForUid(Binder.getCallingUid());
        }
        return false;                                   // file (1), dir (2), root (3): anyone
    }
    public Cursor query(Uri uri, ...) {
        if (i != 1 && i != 2 && i != 3 && isCallerNotAllowed(uri)) {
```

(Case labels folded, comments added.) `insert`, `update` and `delete` use the same check. The table
holds `path`, `path_decrypted`, `file_owner`, `sharees`, `share_by_link`, `metadata_gps`...

**Verdict:** any installed app can read your file names, folders and shares, decrypted names of
end-to-end-encrypted files included, and change or delete rows. [GitHub Security Lab noted it in 2021](https://securitylab.github.com/advisories/GHSL-2021-1007-Nextcloud_Android_app/);
the SQL injection from that report was fixed, the open access is still there in 35.0.1. Read from
code, not run on a device.
</details>

<details>
<summary><b>4. AntennaPod trusts certificates the user installed</b></summary>

<br>

```text
$ apkscope resource antennapod.apk res/xml/network_security_config.xml
    <base-config cleartextTrafficPermitted="true">
        <trust-anchors>
            <certificates src="user" />
            <certificates src="system" />

$ apkscope decompile antennapod.apk de.danoeh.antennapod.net.ssl.CompositeX509TrustManager --method checkServerTrusted
        X509TrustManager x509TrustManager = (X509TrustManager) iterator.next();
        try {
            x509TrustManager.checkServerTrusted(x509CertificateArr, str);
```

**Verdict:** since Android 7 apps ignore user certificates; this one opts back in, so a managed
device or a "VPN" app that installs one can read its HTTPS. The custom TrustManager tries each real
one and throws if none accepts: a deliberate choice, not the trust-everything bug.
</details>

<details>
<summary><b>5. Thunderbird ships a debug activity any app can launch</b> · checked: safe on release</summary>

<br>

`scan` lists Jetpack Compose's `PreviewActivity` as exported. It runs any class named in the intent.

```text
$ apkscope decompile thunderbird.apk androidx.compose.ui.tooling.PreviewActivity --method onCreate
        int i = getApplicationInfo().flags & 2;
        if (i == 0) {
            Log.md(str, "Application is not debuggable. Compose Preview not allowed.");
            finish();
```

**Verdict:** `flags & 2` is `FLAG_DEBUGGABLE`, and release builds aren't. Every scanner flags this;
only the code says it's safe, until someone ships a debuggable build.
</details>

<details>
<summary><b>6. KeePassDX "uses AES in ECB mode"</b> · checked: correct</summary>

<br>

```text
$ apkscope decompile keepassdx.apk com.kunzisoft.encrypt.aes.AESTransformer
            cipher = Cipher.getInstance("AES/ECB/NoPadding");
            key = (Key) new SecretKeySpec(bArr, "AES");
            cipher.init(1, key);
                while (UnsignedKt.ulongCompare(j2, j) < 0) {
```

**Verdict:** the KeePass AES-KDF encrypts one block with itself `rounds` times to slow down password
guessing. ECB on one block is the spec. Knowing which flags to drop is half the job.
</details>

## Speed

<p align="center">
  <img src="docs/bench-open.svg" alt="Open TikTok until fully loaded: apkscope 12.4 s, JADX-GUI 2 min 18 s" width="760">
</p>

| | |
|---|---|
| **11x faster** than JADX to open TikTok, **9x less RAM** | 12.4 s / 997 MB vs 2 min 18 s / 9.4 GB |
| **Up to 1.8x faster** than rust_ASC decompiling one class | faster on 8 of 10 popular apps, level on 2, as Java |
| **1.2-3x faster** decoding the manifest | all 10 apps, with 10-138 MB instead of 26-493 MB |
| **Less memory** than rust_ASC | on 59 of 70 cold queries |
| **2.7x faster** than rust_ASC per agent call | 70 ms vs 191 ms median, over MCP |
| **Typed Java**, not pseudocode | a reviewer, or an agent, can trust it |
| **One tool**: decompiler, xrefs, recon, diff, Frida, unpacking, re-signing, GUI, MCP | instead of five |

Every number comes from a script in [`bench/`](bench). [All results →](BENCHMARKS.md)

**For** security researchers, mobile pentesters, privacy researchers, developers checking what a
library really does, and anyone who hands APKs to an AI agent.

## Get started

**1. Get it:**

Linux (x86_64, aarch64) or macOS — one command, detects your CPU and verifies the checksum:

```bash
curl -fsSL https://raw.githubusercontent.com/suffixing/apkscope/main/scripts/install.sh | sh
# or a pinned version:
curl -fsSL https://raw.githubusercontent.com/suffixing/apkscope/main/scripts/install.sh | sh -s 0.9.1
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/suffixing/apkscope/main/scripts/install.ps1 | iex
```

Or grab a binary straight from [Releases](https://github.com/suffixing/apkscope/releases):

- `apkscope_windows_x86_64.exe` / `apkscope_linux_x86_64` / `apkscope_linux_aarch64` — one
  binary with the viewer, the CLI and the MCP server. On Linux also packaged as
  `apkscope_<version>_<arch>.deb` (Debian/Ubuntu) and `apkscope-<version>-<arch>.AppImage`
  (any distro); those binaries are glibc builds from Ubuntu 24.04, so they need glibc 2.39+.
- `apkscope_linux_musl_x86_64` / `apkscope_linux_musl_aarch64` — the fully static CLI + MCP
  server for servers, containers and musl distros (Alpine); runs on any glibc distro too.
  No viewer: a fully static binary cannot load the desktop's X11/Wayland/GL libraries —
  ask it for the gui and it says exactly that.
- `apkscope_macos_*` — CLI + MCP server; on macOS, build the viewer from source:

```bash
cargo build --release                          # viewer + CLI + MCP server
cargo build --release --no-default-features    # CLI + MCP only (servers)
```

The viewer needs no system GUI libraries at build time: it loads the desktop's X11 or Wayland
stack (and xdg-desktop-portal for file dialogs) at runtime, so the same binary runs on any distro
with a new-enough glibc.

To uninstall, run `scripts/uninstall.sh` (Linux/macOS) or `scripts/uninstall.ps1` (Windows).

**2. Give it to your agent:**

```bash
claude mcp add apkscope -- /path/to/apkscope mcp
```

Any MCP client works: `{ "mcpServers": { "apkscope": { "command": "/path/to/apkscope", "args": ["mcp"] } } }`

**3. Or use it directly:** `apkscope gui app.apk`, or any command below.

## Commands

```bash
# recon
apkscope scan      app.apk                                  # flags, exported components, weak crypto, TLS, keys, URLs
apkscope info      app.apk                                  # permissions, components, deep links, signature
apkscope search    app.apk urls                             # every endpoint (also: secrets, http, code, res)
apkscope diff      old.apk new.apk --strings                # what changed in this update

# follow the code
apkscope refs      app.apk string X-Goog-Api-Key --java     # who uses it, with the Java line
apkscope decompile app.apk com.example.Api --method login   # one method, as Java
apkscope callers   app.apk com.example.Api login            # every caller (callgraph: the tree)
apkscope access    app.apk com.example.Session token        # who reads / writes a field
apkscope decrypt   app.apk                                  # run string decryptors on their constant args

# go further
apkscope dump      app.apk out/ --attach                    # packed app: save the DEX it unpacked in memory
apkscope il2cpp    game.xapk --uses "https://"              # Unity games: which C# method uses a string
apkscope frida     app.apk com.example.Api login            # ready-to-run hook (args, return, throws)
apkscope rename    app.apk "com.a.b#c()" login              # names and comments, kept across runs
apkscope export    app.apk out/                             # Gradle project for Android Studio
apkscope build     app.apk out.apk --string "app_name=My App"   # edit, rebuild, sign (v2 + v3)

# patch the code
apkscope smali     app.apk com.example.Api > Api.smali          # one class as editable Smali
apkscope build     app.apk out.apk --smali Api.smali            # assemble it back in, rebuild, sign
```

`--json`, `--limit N`, `--offset N` work everywhere. Takes `.apk`, `.apks`, `.xapk`, `.dex`.
`apkscope --help` lists everything.

<details>
<summary><b>How decrypt and dump work</b></summary>

<br>

- **`decrypt`** runs the app's own decryptor in a sandboxed Dalvik interpreter (no I/O, no reflection,
  step-limited) and shows the result inline: `a.b("x#9!") /* = "https://api.example.com" */`.
- **`dump`** lets the app unpack itself under Frida, then pulls every DEX out of its memory, skipping
  the ones already on disk. It also hooks native `evalString` to save scripts after the engine
  decrypted them. Needs a rooted device with frida-server.
</details>

## Java you can actually read

<table><tr><th>rust_ASC</th><th>apkscope</th></tr><tr><td>

```java
Throwable v3_5 = ((java.net.HttpURLConnection)
    ((com.google.android.datatransport.cct
      .CctTransportBackend$HttpRequest) p17)
    .a.openConnection());
v3_5.setDoOutput(1);
v3_5.setInstanceFollowRedirects(0);
java.io.InputStreamReader v5_4 = new Object[1];
```

</td><td>

```java
HttpURLConnection httpURLConnection =
    (HttpURLConnection) httpRequest.a.openConnection();

httpURLConnection.setDoOutput(true);
httpURLConnection.setInstanceFollowRedirects(false);
Object[] objArr2 = new Object[1];
```

</td></tr></table>

Loops, `switch`, `try`/`catch`/`finally`, lambdas, enums, typed names and deobfuscation
(`a.b` → `pkg_a.RunnableB`). Faster than rust_ASC per class on 8 of 10 popular apps, level on the
other 2. Uncertain methods get a
`/* WARNING */` header; failures fall back to Smali, never silently.

## Built for AI agents

| MCP tool | What the agent gets |
|---|---|
| `find_refs` | who uses a string, type, method or field, or implements an interface; with Java lines |
| `decompile` | Java or Smali of several classes, or one method |
| `search` | code, strings, usages, classes, resources, URLs, HTTP calls, secrets |
| `apk_info` / `get_manifest` / `diff_apks` | permissions, components, deep links, risky flags, what changed |
| `list_classes` / `list_methods` / `list_strings` | with prefix, filter and app-only |
| `get_resource` / `export_project` / `verify_signature` | resources, Gradle export, signatures |
| `frida_start` / `frida_hook` / `frida_events` / `frida_script` | live Frida: hook by name, read calls, args, returns |
| `callers` / `call_graph` / `field_access` | who calls a method, the callee tree, where a field is read / written |
| `decrypt_strings` / `scan` | recovered string-decryptor plaintext; one-shot URL + secret + HTTP scan |

Replies page with `limit` / `offset` and cap at 60k characters; `app_only` hides androidx, Play
services and ad SDKs. 39 agent calls on TikTok: **3.94 s** vs rust_ASC's **6.54 s**.

Ask *"Can this app's HTTPS traffic be intercepted?"* and the agent finds finding 4 on its own, in
under half a second of tool time.

## The viewer

<p align="center"><img src="docs/viewer-overview.png" alt="apkscope viewer showing AntennaPod's security overview" width="760"></p>

Double-click `apkscope.exe` (or run the Linux binary / .deb / AppImage) and drop in an APK. Security overview first, then your app's code apart
from the libraries. **Click** a name to mark it, **Ctrl+click** to jump to it, **right-click** for
callers, call graph, field access, Frida hook, Smali, rename. Results jump to the exact line.
**Edit smali** on a class to patch its code, then **Build & sign** with undo.

## Honest limits

- Some complex methods still use `goto` labels (marked `WARNING`).
- Cold one-shot queries: rust_ASC is still quicker at string xrefs (0.78-0.97x, 5-40 ms) and a
  little quicker on most ~50 ms listings. In agent sessions apkscope is faster.
- Huge apps take about 1 GB in the viewer; the first full-code search takes a minute, then it's cached.
- The viewer is tested on Windows and Linux (Ubuntu, including WSLg).
- The static musl binary (`apkscope_linux_musl_*`) has no viewer, and cannot have one: a fully
  static binary cannot dlopen() the desktop's X11/Wayland/GL libraries. Use the glibc binary
  or the .deb/AppImage packages for the window.
- Scans are heuristics and findings are leads, not verdicts (see finding 6).
- `decrypt` only runs decryptors that compute from constants.
- `dump` needs a rooted device. Some protectors crash when Frida spawns the app: start it normally
  and use `--attach`. Script capture needs an engine that exports `evalString`, and isn't verified on
  a real game yet (an x86 emulator can't hook ARM-only engines).
- `il2cpp` reads arm64 builds and was checked on metadata v39 (Unity 6); older versions may misparse.
- Callers, call graphs and Frida hooks see direct calls only, not reflection, JNI or dynamic code.

## Written from scratch

DEX parser, decompiler, deobfuscator, resource decoder, search index, APK signer, MCP server and
viewer: all Rust, from zero. Not a JADX fork, not a wrapper.

Wrong output? Open an issue with the class name and what JADX shows. [Apache 2.0](LICENSE);
"apkscope" and its logo are trademarks of Ahmad, see [NOTICE](NOTICE).
`src/data/android_public.txt` comes from AOSP (Apache 2.0).
