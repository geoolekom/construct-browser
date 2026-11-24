#!/usr/bin/env bash
set -euo pipefail

fail() {
  echo "error: $*" >&2
  exit 1
}

[ "${GITHUB_EVENT_NAME:-}" = workflow_dispatch ] &&
  [ "${RELEASE_CONFIRMED:-}" = true ] || fail "dispatch the release explicitly and confirm it"
jq -e --arg repo "${GITHUB_REPOSITORY:-}" 'index($repo) != null' >/dev/null \
  <<< "${RELEASE_REPOSITORIES:-[\"constructorfabric/construct\"]}" &&
  [ "${GITHUB_REF:-}" = refs/heads/main ] &&
  [ "${GITHUB_REF_PROTECTED:-}" = true ] || fail "a release requires protected canonical main"
[[ "${RELEASE_SOURCE_SHA:-}" =~ ^[0-9a-f]{40}$ ]] &&
  [ "$RELEASE_SOURCE_SHA" = "${GITHUB_SHA:-}" ] &&
  [ "$RELEASE_SOURCE_SHA" = "$(git rev-parse HEAD)" ] || fail "the release SHA must match the workflow and checkout"
checkout_status=$(git status --porcelain=v1 --untracked-files=all)
[ -z "$checkout_status" ] || fail "a release requires a clean checkout"

canonical_ref=$(git ls-remote --exit-code https://github.com/$GITHUB_REPOSITORY.git refs/heads/main)
[ "${canonical_ref%%$'\t'*}" = "$RELEASE_SOURCE_SHA" ] || fail "the release SHA is no longer canonical main"

if [ "$#" -eq 0 ]; then
  exit 0
fi
[ "$#" -eq 1 ] || fail "choose one release platform"
case "$1" in
  ios)
    grep -Eq '^[[:space:]]*APP_BUNDLE_ID_IS_FINAL: "YES"$' mobile/iosApp/project.yml \
      || fail "commit the final iOS bundle ID before releasing"
    ;;
  android)
    grep -qx 'constructApplicationIdIsFinal=true' mobile/gradle.properties \
      || fail "commit the final Android application ID before releasing"
    ;;
  *) fail "choose ios or android" ;;
esac

endpoint="repos/$GITHUB_REPOSITORY/environments/mobile-$1-release"
gh api "$endpoint" | jq -e '
  .can_admins_bypass == false and
  .deployment_branch_policy.custom_branch_policies == true and
  any(.protection_rules[]; .type == "required_reviewers" and
    .prevent_self_review == true and (.reviewers | length > 0))
' >/dev/null || fail "the release environment must enforce independent approval and disable administrator bypass"
gh api --paginate "$endpoint/deployment-branch-policies" |
  jq -se '[.[].branch_policies[]] | length == 1 and .[0].type == "branch" and .[0].name == "main"' >/dev/null \
  || fail "the release environment must restrict deployments to main"
