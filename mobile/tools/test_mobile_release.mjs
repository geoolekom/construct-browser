import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { checkTestFlight } from "../scripts/testflight-release.mjs";

const release = { bundle: "test.example.app", version: "1.2.3", build: "260101.1", group: "Internal" };

test("release source must be the explicitly confirmed checkout of protected canonical main", (t) => {
    const fixture = mkdtempSync(join(tmpdir(), "mobile-release-source-"));
    t.after(() => rmSync(fixture, { recursive: true, force: true }));
    writeFileSync(join(fixture, "git"), [
        "#!/bin/bash",
        'case "$*" in',
        '  "rev-parse HEAD") printf "%s\\n" "$TEST_CHECKOUT" ;;',
        '  "status --porcelain=v1 --untracked-files=all") printf "%s" "$TEST_STATUS" ;;',
        '  "ls-remote --exit-code https://github.com/"*".git refs/heads/main")',
        '    printf "%s\\trefs/heads/main\\n" "$TEST_MAIN" ;;',
        "  *) exit 2 ;;",
        "esac",
    ].join("\n"), { mode: 0o700 });
    writeFileSync(join(fixture, "gh"), [
        "#!/bin/bash",
        'case "$*" in',
        '  "api repos/constructorfabric/construct/environments/mobile-ios-release") printf "%s\\n" "$TEST_ENVIRONMENT" ;;',
        '  "api --paginate repos/constructorfabric/construct/environments/mobile-ios-release/deployment-branch-policies")',
        '    printf "%s\\n" "$TEST_BRANCH_POLICIES" ;;',
        "  *) exit 2 ;;",
        "esac",
    ].join("\n"), { mode: 0o700 });
    mkdirSync(join(fixture, "mobile/iosApp"), { recursive: true });
    writeFileSync(join(fixture, "mobile/iosApp/project.yml"), '    APP_BUNDLE_ID_IS_FINAL: "YES"\n');

    const sha = "a".repeat(40);
    const other = "b".repeat(40);
    const script = fileURLToPath(new URL("../scripts/check-ci-release.sh", import.meta.url));
    const base = {
        PATH: fixture + ":" + process.env.PATH,
        GITHUB_EVENT_NAME: "workflow_dispatch",
        GITHUB_REPOSITORY: "constructorfabric/construct",
        GITHUB_REF: "refs/heads/main",
        GITHUB_REF_PROTECTED: "true",
        GITHUB_SHA: sha,
        RELEASE_SOURCE_SHA: sha,
        RELEASE_CONFIRMED: "true",
        TEST_CHECKOUT: sha,
        TEST_MAIN: sha,
        TEST_STATUS: "",
    };
    const guard = (overrides, ...args) => spawnSync("bash", [script, ...args],
        { cwd: fixture, env: { ...base, ...overrides }, encoding: "utf8" });
    assert.equal(guard({}).status, 0);
    const trusted = '["constructorfabric/construct","fork/construct"]';
    assert.equal(guard({ RELEASE_REPOSITORIES: '["fork/construct"]' }).status, 1);
    assert.match(guard({ GITHUB_REPOSITORY: "fork/construct", RELEASE_REPOSITORIES: '[]' }).stderr, /canonical main/);
    assert.equal(guard({ GITHUB_REPOSITORY: "fork/construct", RELEASE_REPOSITORIES: trusted, TEST_MAIN: other }).status, 1);
    for (const [name, value, error] of [
        ["GITHUB_EVENT_NAME", "pull_request", /dispatch/],
        ["RELEASE_CONFIRMED", "false", /confirm/],
        ["GITHUB_REPOSITORY", "fork/construct", /canonical main/],
        ["GITHUB_REF", "refs/heads/feature", /canonical main/],
        ["GITHUB_REF_PROTECTED", "false", /protected/],
        ["RELEASE_SOURCE_SHA", other, /SHA must match/],
        ["TEST_CHECKOUT", other, /SHA must match/],
        ["TEST_MAIN", other, /no longer canonical main/],
        ["TEST_STATUS", " M mobile/iosApp/project.yml", /clean checkout/],
    ]) {
        const result = guard({ [name]: value });
        assert.equal(result.status, 1, name);
        assert.match(result.stderr, error, name);
    }
    const policy = {
        can_admins_bypass: false,
        deployment_branch_policy: { custom_branch_policies: true },
        protection_rules: [{ type: "required_reviewers", prevent_self_review: true, reviewers: [{ type: "User" }] }],
    };
    const branches = { branch_policies: [{ type: "branch", name: "main" }] };
    const dispatch = (environment, branchPolicies = branches) => guard({
        TEST_ENVIRONMENT: JSON.stringify(environment),
        TEST_BRANCH_POLICIES: JSON.stringify(branchPolicies),
    }, "ios");
    assert.equal(dispatch(policy).status, 0);
    for (const invalid of [
        { ...policy, can_admins_bypass: true },
        { ...policy, can_admins_bypass: undefined },
        { ...policy, protection_rules: [] },
        { ...policy, protection_rules: [{ ...policy.protection_rules[0], prevent_self_review: false }] },
    ]) {
        const result = dispatch(invalid);
        assert.equal(result.status, 1);
        assert.match(result.stderr, /independent approval/);
    }
    const broad = dispatch(policy, { branch_policies: [{ type: "branch", name: "*" }] });
    assert.equal(broad.status, 1);
    assert.match(broad.stderr, /restrict deployments to main/);
});

test("CI export preserves archive metadata and propagates TestFlight verification failure",
    { skip: process.platform !== "darwin" }, (t) => {
        const verificationStatus = 47;
        const { result, calls } = runIosExport(t, release, verificationStatus);

        assert.equal(result.status, verificationStatus, result.stderr + result.stdout);
        assert.deepEqual(calls, ["unused", "unused", "export manageAppVersionAndBuildNumber=false", "verify"]);
    });

function runIosExport(t, { bundle, version, build }, verificationStatus) {
    const fixture = mkdtempSync(join(tmpdir(), "mobile-release-export-"));
    t.after(() => rmSync(fixture, { recursive: true, force: true }));
    for (const path of ["bin", "scripts", "iosApp", "tmp"]) mkdirSync(join(fixture, path));
    const script = join(fixture, "scripts/build-ios.sh");
    copyFileSync(new URL("../scripts/build-ios.sh", import.meta.url), script);
    writeFileSync(join(fixture, "iosApp/project.yml"), [
        "APP_BUNDLE_ID: " + bundle, 'APP_BUNDLE_ID_IS_FINAL: "YES"', "DEVELOPMENT_TEAM: fixture",
        'MARKETING_VERSION: "' + version + '"', 'CURRENT_PROJECT_VERSION: "' + build + '"',
    ].join("\n") + "\n");
    const archiveDir = join(fixture, "build/ios-archive-check/Construct.xcarchive/Products/Applications/Construct.app");
    mkdirSync(archiveDir, { recursive: true });
    const plist = join(archiveDir, "Info.plist");
    writeFileSync(plist, '<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict>' +
        "<key>CFBundleIdentifier</key><string>" + bundle + "</string>" +
        "<key>CFBundleShortVersionString</key><string>" + version + "</string>" +
        "<key>CFBundleVersion</key><string>" + build + "</string></dict></plist>");

    const stub = (path, body) => writeFileSync(join(fixture, path), "#!/bin/bash\nset -eu\n" + body + "\n",
        { mode: 0o700 });
    stub("scripts/check-ci-release.sh", "exit 0");
    for (const tool of ["git", "xcodegen", "sleep"]) stub("bin/" + tool, "exit 0");
    stub("bin/xcode-select", "echo /Applications/Xcode.app/Contents/Developer");
    stub("bin/xcrun", [
        'case "$2" in',
        '  list) echo "    iPhone Test (AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA) (Booted)" ;;',
        '  io) echo "fixture screenshot" > "$5" ;;',
        "esac",
    ].join("\n"));
    stub("bin/xcodebuild", [
        'if [ "$1" = "-exportArchive" ]; then',
        '  flag=$(/usr/libexec/PlistBuddy -c "Print :manageAppVersionAndBuildNumber" "$5")',
        '  echo "export manageAppVersionAndBuildNumber=$flag" >> "$TRACE"',
        "  exit 0",
        "fi",
        'while [ "$#" -gt 0 ]; do',
        '  if [ "$1" = "-archivePath" ]; then',
        '    app="$2/Products/Applications/Construct.app"',
        '    mkdir -p "$app"',
        '    cp "$PLIST" "$app/Info.plist"',
        "    exit 0",
        "  fi",
        "  shift",
        "done",
        "mkdir -p build/ios/Build/Products/Debug-iphonesimulator/Construct.app",
    ].join("\n"));
    stub("bin/node", [
        'echo "$2" >> "$TRACE"',
        'if [ "$2" = verify ]; then exit "$VERIFY_STATUS"; fi',
    ].join("\n"));

    const trace = join(fixture, "trace");
    writeFileSync(trace, "");
    const result = spawnSync("bash", [script, "--ci-release"], {
        env: {
            PATH: join(fixture, "bin") + ":" + process.env.PATH,
            TMPDIR: join(fixture, "tmp"),
            JAVA_HOME: fixture,
            APP_STORE_CONNECT_KEY_PATH: plist,
            APP_STORE_CONNECT_KEY_ID: "fixture",
            APP_STORE_CONNECT_ISSUER_ID: "fixture",
            TRACE: trace,
            PLIST: plist,
            VERIFY_STATUS: String(verificationStatus),
        },
        encoding: "utf8",
    });
    return { result, calls: readFileSync(trace, "utf8").trim().split("\n") };
}

const accepted = { id: "build", attributes: { version: release.build, processingState: "VALID" } };

function appleFixture(overrides = {}) {
    const routes = {
        "/v1/apps": { data: [{ id: "app", attributes: { bundleId: release.bundle } }] },
        "/v1/betaGroups": { data: [{ id: "group", attributes: { isInternalGroup: true, name: release.group } }] },
        "/v1/builds": { data: [] },
        "/v1/apps/app/buildUploads": { data: [] },
        "/v1/builds/build/preReleaseVersion": { data: { attributes: { version: release.version, platform: "IOS" } } },
        "/v1/builds/build/buildBetaDetail": { data: { attributes: { internalBuildState: "IN_BETA_TESTING" } } },
        "/v1/betaGroups/group/builds": { data: [{ id: "build" }] },
        ...overrides,
    };
    return async (route, params) => {
        assert.ok(Object.hasOwn(routes, route), route);
        if (route === "/v1/builds") {
            assert.deepEqual(params, { "filter[app]": "app", "filter[version]": release.build });
        }
        return routes[route];
    };
}

test("preflight rejects both processed builds and pending uploads for the build number", async () => {
    await checkTestFlight(appleFixture(), "unused", release);
    await assert.rejects(checkTestFlight(appleFixture({
        "/v1/builds": { data: [accepted] },
    }), "unused", release), /already uploaded/);
    await assert.rejects(checkTestFlight(appleFixture({
        "/v1/apps/app/buildUploads": { data: [{ attributes: { cfBundleVersion: release.build } }] },
    }), "unused", release), /already uploaded/);
});

test("verification requires the exact iOS version and availability in the intended internal group", async () => {
    const processed = { "/v1/builds": { data: [accepted] } };
    assert.equal(await checkTestFlight(appleFixture(processed), "verify", release), true);
    for (const override of [
        { "/v1/betaGroups/group/builds": { data: [] } },
        { "/v1/builds/build/buildBetaDetail": { data: { attributes: { internalBuildState: "READY_FOR_BETA_TESTING" } } } },
    ]) {
        assert.equal(await checkTestFlight(appleFixture({ ...processed, ...override }), "verify", release), false);
    }
    await assert.rejects(checkTestFlight(appleFixture({
        ...processed,
        "/v1/builds/build/preReleaseVersion": { data: { attributes: { version: "1.2.2", platform: "IOS" } } },
    }), "verify", release), /different version or platform/);
});
