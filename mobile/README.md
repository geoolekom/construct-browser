# Construct mobile

Construct on the phone: an Android app and an iPhone app that share their logic in Kotlin
Multiplatform. This folder holds the shared Gradle project.

## Layout

| Module              | What it holds                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| ------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `core/model`        | The types: conversation, message, fact, dashboard.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `core/construct`    | The one interface the screens use for chats, memory and the dashboard, and `FakeConstruct`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| `core/database`     | The phone's copy in SQLite (SQLDelight), behind the same interface. Sending says "not available yet".                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `core/designsystem` | The tokens both apps share, as plain numbers: the colour roles of the desktop app's kit in light and dark, and a scale of spacing steps. Text sizes, radii and control sizes belong to each app's own theme.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `core/chat`         | The chat core (Rust) as Kotlin sees it: `ChatCore` with the request body of the fixed test conversation and the Test of a model setting. Android calls the shared library through JNA, the iPhone reads the C header with cinterop. See "The chat core". |
| `core/keys`         | The model setting on the phone: `ModelSettingsStore` keeps the address and the model name in the phone's own storage and the API key in the Keychain (iPhone) or under a Keystore key (Android), with `FakeModelSettingsStore` and a contract test every store passes. See "The model setting and its key". |
| `feature/template`  | An empty feature to copy: a state, a state holder (plain common Kotlin that both apps watch), an Android screen that shows the state and loads nothing, and a test with `FakeConstruct`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `feature/memory`    | The first feature made from the template: the person's facts as a plain list.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `feature/settings`  | The model setup of the Settings page, made from the template: a state, a state holder (read, save, test, remove the key) and the Android screen with its form row and message line.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `shared`            | The one Kotlin framework the iPhone app links (`ConstructShared`): it re-exports the modules above, adds `UiScope` (a main-thread scope Swift can create and close) and uses SKIE, which shows Kotlin flows to Swift as `AsyncSequence`. Its analytics upload is off.                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| `androidApp`        | The Android app. `Composition.kt` is the one place its objects are built (the copy in the app; `FakeConstruct` would replace it there), `ConstructTheme.kt` maps the shared colours to the Compose theme and holds the Android text sizes and radii. Debug builds fill an empty copy with the sample data; release builds do not. `AppFrame.kt` is the frame of every page (top bar with the title, a connection line, the navigation bar); a page without a feature is a `PlaceholderPage` until a feature module takes it over.                                                                                                                                                                           |
| `iosApp`            | The iPhone app in SwiftUI. `project.yml` generates the Xcode project (not committed); Swift files under `App/`, `Core/` and `Features/` are picked up by folder. `Composition.swift` is the one place its objects are built, `Core/FlowState.swift` is the one adapter every feature uses to watch a state holder, `Core/ConstructTheme.swift` maps the shared colours to SwiftUI and holds the iPhone text sizes, radii and control sizes. `App/AppFrame.swift` is the frame of every page (the tab bar, a navigation bar with the inline title, a connection line); a page without a feature is a `PlaceholderPage` until a feature folder takes it over. The tab icons are Lucide, in the asset catalog. |

The state holders wrap the one-shot reads of `Construct`: a fact changed elsewhere shows after `refresh()`, not by itself. A read that is still running is dropped when a new one starts. A `Construct` reports a failed read as `Outcome.Failed` and does not throw; run a holder's scope and call `refresh()` on the main thread.

The calls and names follow the desktop app's `chat-protocol.ts`, `memory-protocol.ts` and
`dashboard-protocol.ts` in `app/src`. A screen talks to `Construct` and never knows where the data
comes from. `FakeConstruct` has seeded data and a canned streamed answer, for tests and previews.

The copy keeps the dashboard types and the fact values as JSON text, and enums by name. Removing
a field needs no migration; adding a required field or renaming a field or a constant does
(`core/database`), like a column change.

Targets: Android (minimum API 26, set once in `gradle/libs.versions.toml`), `iosArm64` and
`iosSimulatorArm64`. Every version lives in `gradle/libs.versions.toml`.

## The chat core

The chat is one Rust library, the `chat-core` crate, shared with the desktop agent. The `chat-bridge`
crate puts a plain C interface on it (`chat-bridge/include/construct_chat.h`), and the shared Kotlin
code calls that through `core/chat`:

- `ChatCore.fixedRequestBody(toolsFile)` builds the request body of a fixed test conversation. The
  bytes are the ones the provider's prompt cache sees, and equal the stored files of
  `chat-core/tests/golden`.
- `ChatCore.testModel(baseUrl, model, apiKey)` tests a model setting and returns `Works` or
  `Failed(reason)`, the reason in the desktop app's words. The address check of the core refuses a
  plain `http` address that is not on `localhost` or `127.0.0.1`. The key is an argument only: it is in
  no log and in no returned text.

Why a C interface: Kotlin/Native cannot load the Kotlin bindings that Rust bridge generators write,
and a C header is read by cinterop as it is. Android calls the same functions by name through JNA
(no glue code to write for JNI; the one exception is the JNI function that gives Android's certificate
check the JVM, see `https` on Android below). One interface, no generator, and a streamed answer will cross the
same way, as a C function pointer with a context.

`scripts/build-core.sh [ios|android|all]` builds the core once for each target into a folder outside
the repository, `CONSTRUCT_CORE_BUILD_DIR` (default `~/Library/Caches/construct-chat-core`), where the
cargo cache is too unless `CARGO_TARGET_DIR` is set:

```text
<dir>/ios/device/libconstruct_chat_bridge.a        iPhone device (aarch64-apple-ios)
<dir>/ios/simulator/libconstruct_chat_bridge.a     iPhone Simulator (aarch64-apple-ios-sim)
<dir>/android/<abi>/libconstruct_chat_bridge.so    arm64-v8a and x86_64
```

The Gradle tasks `buildChatCoreIos` and `buildChatCoreAndroid` run the script, and the iPhone
framework and the Android app depend on them, so Xcode and Gradle builds build the core first. Their
inputs are the Rust sources, `Cargo.lock`, the script and the two files it reads the platform versions
from, and their outputs are the libraries, so a
build with unchanged sources does not start cargo. Nothing compiled is committed.

What a Mac needs for the Rust part, in addition to the list below:

- `rustup` and the Rust toolchain of `rust-toolchain.toml`, with the four targets:
  `rustup target add aarch64-apple-ios aarch64-apple-ios-sim aarch64-linux-android x86_64-linux-android`
- `cargo-ndk`, which builds a Rust library for each Android ABI with the NDK's compiler (the bridge itself has no generator): `cargo install cargo-ndk`
- The Android NDK, any recent release (27.2 was proven): `sdkmanager 'ndk;27.2.12479018'`. The script takes
  `ANDROID_NDK_HOME`, or else the newest one under `$ANDROID_HOME/ndk`
- `cmake`, which the build script of the TLS library may ask for: `brew install cmake`
- Xcode, the app (the command line tools carry no iPhone SDK), as in the list below

The first build compiles every crate the core uses for each of the four targets and takes a quarter
of an hour on an Apple Silicon laptop; a second one with nothing changed does nothing. `./gradlew :androidApp:connectedDebugAndroidTest` runs the chat core's tests on a running emulator,
by hand: the workflow only compiles them, because it starts no emulator. `./gradlew
:core:chat:iosSimulatorArm64Test` runs them in the iPhone Simulator, and the workflow runs that one.

The Android minimum API level and the iPhone deployment target of the Rust build are read from
`gradle/libs.versions.toml` and `iosApp/project.yml`, and both files are inputs of the Gradle tasks.
`rustSources` in `build.gradle.kts` and the paths of the workflow list the crates the library is built
from. The script stops when `cargo metadata` shows other crates than `chat-bridge`, `chat-core` and
`connectors` (the list it keeps), so a new path dependency cannot leave those two lists stale.

`https` on Android: the TLS stack checks certificates through Android's own trust store, which it
reaches through the JVM. The app calls `initChatCore(context)` (`core/chat`, in `Application.onCreate`)
once, which hands the context to the Rust library through one JNI function. If the init fails, or the
library does not load, the app logs it and goes on, since the memory screens do not need the core. The
Kotlin part of the check, `org.rustls:rustls-platform-verifier`, is an AAR that its authors publish as a Maven repository
on GitHub (`settings.gradle.kts` limits that repository to its own group). Its version is read from
`Cargo.lock`, so it follows the crate. A call made before a successful init
answers "unreachable" for an `https` address. If the library itself did not load, every call throws
`UnsatisfiedLinkError`: the library is built for `arm64-v8a` and `x86_64` only, so the app keeps only
those two (`abiFilters` in `androidApp/build.gradle.kts`, which also drops the 32-bit libraries of JNA) and a
32-bit device cannot install it.

The first `https` call of a process was slow on the emulator: 10 s, and once more than the core's
15 s deadline, so it came back as `timeout`; the next calls took under half a second. The emulator has
no DNS, and it is not known whether a real phone has the stall. Check it on a device before the chat
depends on it.
`AndroidTlsTest` shows the system check at work (and that `ConstructApp` started it): a local TLS
server with a certificate no phone trusts is refused with a TLS alert. The Gradle build stops when
`Cargo.lock` holds two copies of the verifier. No test reaches a real provider.

## The model setting and its key

`core/keys` keeps what the person typed on the Settings page: the address of the model, its name and the
API key. `ModelSettingsStore` is the one interface; a screen's state holder reads and writes it and the
screen never sees it. A setting read back says only that a key is set (`ModelSettings.keyIsSet`); the whole
key comes out through `readKey()` for a call to the model and is never part of a state, a log line, an
error text or a copy. Every store call waits for the disk: the holder runs it off the main thread. A call
that fails throws `ModelSettingsException` with a fixed plain reason.

- Android: the address and the model name sit in a private preferences file. The key sits in a file under
  `noBackupFilesDir`, which the system never backs up, encrypted with AES-GCM under a 256-bit Keystore key
  that cannot be taken out of the Keystore. `res/xml/backup_rules.xml` and `data_extraction_rules.xml`
  exclude the key file and the preferences too, should `allowBackup` ever be turned on (`BackupRulesTest`
  reads them). A key that cannot be opened (a restored phone has no Keystore key) is reported and replaced
  by saving a new one.
- iPhone: the same text storage in `UserDefaults`; the key is a generic-password Keychain item with
  `kSecAttrAccessibleWhenUnlockedThisDeviceOnly` and `kSecAttrSynchronizable` false, so it is in no backup and not
  in iCloud Keychain. Saving removes the old item and adds a new one.

Tests: `ModelSettingsStoreContract` (`core/keys`, common) is what every store does; the fake, the shared
store over memory and the encrypted file (JVM, with an AES key of the JVM in place of the Keystore key) pass it.
On the phone, `ModelSetupTest` (`./gradlew :androidApp:connectedDebugAndroidTest`, an emulator) runs the real
Settings page against a fake provider on a local port. `iosApp/Tests/ModelSettingsStoreTests.swift` runs the
iPhone store over the real Keychain; the Keychain exists only inside the app, so it is an app test, not a
Kotlin one. `scripts/check-key-not-leaked.sh android|ios` saves a made-up key through the app, proves its
search by planting the key in a file, and then looks for the key in the app's files and in the log.

## Rules

Data flows one way: the store, the state holder, the screen. A screen shows a state and passes on
the user's actions; it loads nothing. Three rules are checked by `tools/check_structure.py`, which
reads Kotlin, Gradle and Swift files and prints `file:line: message` for each violation. They fail
`./gradlew check` and `iosApp/check.sh`, which `.github/workflows/mobile.yml` runs.

1. A feature uses no other feature. In Gradle: no `project(":feature:x")` or `projects.feature.x` of
   another feature. In Kotlin: no mention of another feature's package. In Swift: a file under
   `iosApp/Features/<Name>` names no public top-level type that another feature declares, in Swift
   (`struct`, `class`, `enum`, `protocol`, `actor`, `typealias`, with any modifiers and attributes
   in any order but not `private` or `fileprivate`) or in Kotlin (`class`, `interface`, `object`,
   `typealias`, with any modifiers but not `private`, `internal` or `protected`). Swift sees all
   Kotlin through `ConstructShared`, so it is matched by name. A type the file declares itself,
   nested or private, is its own in that file, and a nested Kotlin type is its feature's own: Swift
   shows it qualified (`Outer.MemoryState`) and the check cannot tell a qualified use from a bare
   one, so it lets the bare name pass. Name a feature's public types after the feature
   (`MemoryState`, not `State`),
   because a bare name that another feature declares also fails in a feature that uses the
   SwiftUI or core name. A feature may use `core/*`. A feature folder name maps between Kotlin and
   Swift without `_`, `-` and case: `shopping_cart` is `ShoppingCart`.
1. A screen names no store, state holder or `Construct` type. A screen is any file with `Screen` in
   its name: in a feature's `src/*Main` source sets for Kotlin, under `iosApp/Features/<Name>` for
   Swift. Each feature folder that exists on a side (a `feature/<name>` module, an
   `iosApp/Features/<Name>` folder) needs at least one there, so a renamed screen or a feature with
   no screen is an error (`required: …`), not a way out. Names ending in `Construct`, `Holder`, `Api`
   (but not `Experimental…Api`), `Repository`, `Store`, `Dao`, `Database` or `Model`, and `UiScope`,
   are refused, and a Kotlin screen imports no `core.construct` or `core.database` package. The
   holder is built outside the screen (on the iPhone in `<Name>Page.swift`) and the screen gets its
   state.
1. A screen has no colour or size literal: no `16.dp`, `14.sp`, `Color(...)`, `0xFF...`,
   `Color.Red`, `Color(red:...)`, `.red`, `.padding(8)`, `.frame(width: 320)`, `CGFloat(24)`. Colours
   and spacing come from `core/designsystem`; text sizes, radii and control sizes come from the
   app's own theme.

A fourth rule, that spacing, colours, text sizes and radii are set once in the theme of each app,
is reviewed, not checked.

What the checks do not catch: they are regular expressions over text, not a parser. All of these
pass: a literal that does not look like the patterns above (a size computed from a number, a colour
from a hex string at run time, `0xFF000000L`, `(8).dp`); a store type with a name outside the list;
a feature that reaches another feature through a top-level function or a type declared in the middle
of a line; and code inside a string interpolation (`"${Color.Red}"`, `"\(CGFloat(4))"`), which is
not scanned. A string whose interpolation holds an escaped quote can also hide the rest of that one
line. The check fails when it finds no feature or no `iosApp/Features` folder, so a moved folder is
an error, not a pass.

`./gradlew check` runs the script on the real tree (`checkStructure`) and its tests
(`checkStructureTests`). The tests show each pattern and each name a pattern lists failing on a small
fixture tree built in a temporary folder (no real file is touched), what the comment and string
stripping hides and does not hide, the command's exit status and output, and that a copy of
`feature/template` passes with no other file changed.

## Add a feature

1. Copy `feature/template` to `feature/<name>` and rename what has `template` or `Template` in its
   name or text: the folder, the Kotlin package, the namespace in `build.gradle.kts`, and the
   `TemplateState`, `TemplateHolder` and `TemplateScreen` files and classes. Nothing else needs a
   change for the checks or for the iPhone framework: `settings.gradle.kts` includes every folder
   under `feature/` that has a build file, and `shared/build.gradle.kts` links every feature but
   the template, which is only there to copy.
1. Fill the state, the holder (what it reads from `Construct`) and the Android screen. Keep the test
   with `FakeConstruct`.
1. For the iPhone, copy `iosApp/Features/Memory` to `iosApp/Features/<Name>` and rename it the same
   way: `<Name>Screen.swift` draws a state, `<Name>Page.swift` builds the holder. The Xcode project
   picks the files up by folder.
1. Show it in the app: add the dependency on `:feature:<name>` in `androidApp/build.gradle.kts` and
   put the screen in the navigation of `MainActivity.kt` and `iosApp/App/RootView.swift`.
1. Run `./gradlew check` (it runs the structure checks) and `./iosApp/check.sh` (the same checks, then
   SwiftLint, for a change that touches only Swift).

## What the Mac needs

| Need                                                                                                                                                                                | What it costs                                                                                                |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| An Apple Silicon Mac                                                                                                                                                                |                                                                                                              |
| Xcode (the app, not only the Command Line Tools) and an iOS simulator runtime                                                                                                       | Xcode 27.2 with the iOS 27.2 runtime; several GB                                                             |
| JDK 17, with `JAVA_HOME` set to it                                                                                                                                                  | the version the build was proven with                                                                        |
| The Android SDK (`ANDROID_HOME`, or `sdk.dir` in an uncommitted `local.properties`) with platform 37.2, build tools and the emulator image `android-37.0` `google_apis` `arm64-v8a` | several GB                                                                                                   |
| `xcodegen` and `swiftlint`: `brew install xcodegen swiftlint`                                                                                                                       | 2.48.0 and 0.65.1 were used                                                                                  |
| Python 3.9 or newer, for the structure checks                                                                                                                                       |                                                                                                              |
| Disk for the caches                                                                                                                                                                 | the Gradle caches about 2 GB; the Kotlin/Native toolchain about 1.6 GB, downloaded by the first iPhone build |
| A network for the first build                                                                                                                                                       | public packages only; no account and no token                                                                |

Every version of a library is in `gradle/libs.versions.toml`.

## Build and check

```sh
cd mobile
export JAVA_HOME=$(/usr/libexec/java_home -v 17)
./gradlew check                                  # tests, ktlint, detekt, the structure checks
./gradlew ktlintFormat                           # fix ktlint findings
python3 -I tools/check_structure.py              # only the structure rules, all of them
./iosApp/check.sh                                # the iPhone check: the structure rules, then SwiftLint
./gradlew :androidApp:assembleDebug              # build the Android app without installing it
./gradlew :shared:linkDebugFrameworkIosSimulatorArm64  # link the Kotlin framework for the Simulator
```

`.github/workflows/mobile.yml` runs these commands, and the Simulator build of "Run the iPhone app", for every
pull request that changes `mobile/` and for every merge to `main`.

## Run the Android app

Start an emulator (create one first with `avdmanager` or Android Studio from the image above), then
install and open the app. Debug builds fill an empty copy with the sample data.

```sh
emulator -avd <name> -no-audio &                 # add -no-window to run it hidden
adb wait-for-device shell 'while [ "$(getprop sys.boot_completed)" != 1 ]; do sleep 1; done'
cd mobile
./gradlew :androidApp:installDebug
adb shell am start -n cf.construct.mobile/cf.construct.mobile.android.MainActivity
```

## Run the iPhone app

A Simulator build links the debug `iosSimulatorArm64` framework and a device build the release
`iosArm64` framework (`KOTLIN_FRAMEWORK_DIR` in `iosApp/project.yml`). Xcode builds the Kotlin
framework first, through Gradle, so `JAVA_HOME` should point at JDK 17 (the version the build was
proven with) in the shell that runs `xcodebuild`.
The first build downloads the Kotlin/Native toolchain and is slow.

```sh
cd mobile/iosApp
xcodegen generate                                # writes Construct.xcodeproj (ignored by git)
xcodebuild -project Construct.xcodeproj -scheme Construct \
  -destination 'platform=iOS Simulator,name=<a simulator you have>' test
xcrun simctl boot '<a simulator you have>'       # skip if it is booted
xcodebuild -project Construct.xcodeproj -scheme Construct \
  -destination 'platform=iOS Simulator,name=<a simulator you have>' \
  -derivedDataPath build build
xcrun simctl install booted build/Build/Products/Debug-iphonesimulator/Construct.app
xcrun simctl launch booted "$(sed -n 's/^ *APP_BUNDLE_ID: *//p' project.yml)"
```

`xcrun simctl list devices available` shows the simulator names. Open `Simulator.app` to see the
screen, or run `xcrun simctl io booted screenshot shot.png`.

## Android release

```sh
cd mobile
scripts/release-android.sh
```

The script builds the release APK, signs it, and checks the signature with `apksigner`. The key
comes only from the environment (the keystore path must be absolute): `ANDROID_KEYSTORE_PATH`, `ANDROID_KEY_ALIAS`,
`ANDROID_KEYSTORE_PASSWORD` and, for a keystore whose key has its own password,
`ANDROID_KEY_PASSWORD`. Without the key the script stops and leaves no APK. Only the v2 signature
scheme is used.

The application id and the flag that says whether it is the final one are in `gradle.properties`
(`constructApplicationId`, `constructApplicationIdIsFinal`).

- While the id is not final, nothing is published. The script installs the APK on the running
  emulator or device (it first uninstalls the app, because an APK signed with another key cannot be installed over it; that erases its data) and
  saves a light and a dark screenshot in a folder it prints.
- When the id is final, the script publishes the signed APK as a GitHub pre-release. The tag is
  `android-v<constructVersionName>` and points at the pushed commit the APK was built from; the
  script stops when the tree has changes, the commit is not pushed, the release already exists, or the version code is not higher than that of an earlier release.
  Raise `constructVersionName` and `constructVersionCode` in `gradle.properties` for each release.
  With `RELEASE_DRAFT=1` (or `true`, `yes`) the release is a draft.

## iPhone build

```sh
cd mobile
scripts/build-ios.sh
```

The bundle id and the flag that says whether it is the final one are in `iosApp/project.yml`
(`APP_BUNDLE_ID`, `APP_BUNDLE_ID_IS_FINAL`), with the Apple team (`DEVELOPMENT_TEAM`). `JAVA_HOME`
should point at JDK 17; without it the script looks for one, also in Homebrew.
`IOS_SIMULATOR='<name>'` picks the simulator; by default it is the first single-screen iPhone
(`xcrun simctl list devices available` lists them).

- While the id is not final, nothing is sent to Apple and no key is read. The script builds the app
  for an iPhone simulator, installs and launches it, and saves a light and a dark screenshot in a
  folder it prints. It boots the simulator when none is running and shuts it down afterwards.
- When the id is final, the script first makes the same Simulator build and screenshots, for those
  who do not have the TestFlight build. Then it archives the Release build for a device with
  automatic signing and sends it to TestFlight; a failed screenshot stops the run before anything
  is sent. Set the exact TestFlight build number in `CURRENT_PROJECT_VERSION` in
  `iosApp/project.yml` as part of the feature pull request. Use `<YYMMDD>.<count of the day>`
  in UTC (for example, `261122.2`). Before merging, check current `main` and App Store Connect
  and update the pull request if the number would collide or fall behind. Fetch `origin/main`
  and run the upload from a clean checkout of the merged commit. Before uploading, the script checks
  the archive's version and build number against the project and leaves the project file unchanged.
  Keep `MARKETING_VERSION` unless the release calls for a new app version. The
  upload uses the App Store Connect API key when `APP_STORE_CONNECT_KEY_PATH` (the `.p8` file, kept
  outside the repository), `APP_STORE_CONNECT_KEY_ID` and `APP_STORE_CONNECT_ISSUER_ID` are all set,
  and otherwise the Apple account Xcode is signed in with (Xcode, Settings, Accounts). With neither,
  or with only some of the three variables, it stops before building. The key values are passed to
  `xcodebuild` and never printed.

Run `scripts/build-ios.sh --archive-only` to build an unsigned device archive in
`build/ios-archive-check` and verify its version and build number locally. This mode skips the
Simulator and does not sign in to App Store Connect or upload.

App Store Connect processes a build for some minutes. It then shows in TestFlight for the testers of
an internal group; with automatic distribution on that group, every new build reaches them by
itself. A tester installs it from the TestFlight app on the iPhone.

The first upload from a Mac needs a person once:

- Xcode is signed in with the Apple ID of a member of the team. The Developer role is enough: the
  build is signed for distribution by the team's cloud-managed certificate, so no distribution key
  is on the Mac. If that signing is refused, the role lacks "Access to Cloud Managed Distribution
  Certificate" (App Store Connect, Users and Access).
- The archive signs with the person's local "Apple Development" key, and macOS asks for the login
  keychain password. Click "Always Allow": "Allow" covers one signing only, so the next build asks
  again, and a build with nobody at the Mac waits for it. A new development certificate asks once
  more.
