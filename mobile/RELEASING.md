# Signed mobile releases

Use the Mobile release workflow on canonical `main`. It accepts one platform, the full SHA of
the current `main` commit, and explicit confirmation. Commit the intended release version and
build number to `main` before dispatching. The release stops if `main` advances before upload.

Release jobs run only in repositories named by the `RELEASE_REPOSITORIES` Actions variable, a JSON
array such as `["constructorfabric/construct"]`. Unset, it allows only the canonical repository.
A repository listed there is treated as canonical: its protected `main` is the release source.

Configure these GitHub Actions environments on the canonical repository before adding any
secrets. Restrict deployments to the protected `main` branch, require an independent reviewer,
enable prevention of self-review, and disable bypassing the environment protection rules.
Keep these credentials as environment secrets.

| Environment              | Secrets                                                                                                      | Variable                    |
| ------------------------ | ------------------------------------------------------------------------------------------------------------ | --------------------------- |
| `mobile-ios-release`     | `APP_STORE_CONNECT_KEY_P8_BASE64`, `APP_STORE_CONNECT_KEY_ID`, `APP_STORE_CONNECT_ISSUER_ID`                 | `TESTFLIGHT_INTERNAL_GROUP` |
| `mobile-android-release` | `ANDROID_KEYSTORE_BASE64`, `ANDROID_KEY_ALIAS`, `ANDROID_KEYSTORE_PASSWORD`, optional `ANDROID_KEY_PASSWORD` |                             |

Use an App Store Connect team key with access to the app, its build and upload history, and
the intended internal group. Set `TESTFLIGHT_INTERNAL_GROUP` to the exact group name and enable
automatic distribution of new builds to it. The workflow verifies the unsigned and signed
archive metadata against `iosApp/project.yml`. It checks the build number against App Store
Connect before signing and again before upload, then waits for processing and internal testing.
If verification fails after export, check App Store Connect before dispatching again.

For Android, commit the final application ID and a new version name and code in
`gradle.properties`. The Android publish job creates a pre-release from the checked-out commit.

The PR job tests release guards with fixture data. For a local iPhone build and upload, see
`build-ios.sh` in [the mobile README](README.md).
