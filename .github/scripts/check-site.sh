#!/usr/bin/env bash
# Claims on the site that the code has outgrown: run before the site is published (and
# by hand from the repository root). Checks meaning, not wording, so copy can change.
set -euo pipefail
# From the repository root, wherever it is run from.
cd "$(dirname "$0")/../.."

page=site/browser/index.html
failed=0
fail() {
    echo "check-site: $1" >&2
    failed=1
}

# Browser threads have had a spending limit since 0.1.4.
if grep -qiE 'no (spending|dollar|spend) limit' "$page"; then
    fail "$page says browser threads have no spending limit; they have an optional one per thread"
fi

# The FAQ's cost answer: what follows its question, up to the end of its <details>.
cost=$(awk '/<summary>What does it cost[?]<\/summary>/ { on = 1 } on { print } on && /<\/details>/ { exit }' "$page" | tr '\n' ' ')
if [ -z "$cost" ]; then
    fail "$page has no \"What does it cost?\" question"
else
    grep -qi 'limit' <<<"$cost" || fail "the cost answer does not mention the spending limit"
    grep -qi 'model page' <<<"$cost" || fail "the cost answer does not say where the limit is set (the thread's model page)"
    grep -qiE 'optional|empty|no limit|blank' <<<"$cost" || fail "the cost answer does not say the limit is optional"
fi

# The Chrome Web Store needs a privacy policy at a working address.
[ -s site/privacy/index.html ] || fail "site/privacy/index.html (the privacy policy the store links to) is missing"
grep -q 'Limited Use' site/privacy/index.html || fail "the privacy policy has no Limited Use statement"

# No em dashes in what the site says (its section numbers use them as a design mark).
# grep reads all of it (no -q): under pipefail, sed cut off by an early exit would fail
# the pipeline and hide the match.
dashes=$(for file in site/*.html site/*/*.html; do
    sed 's/<span class="num">[^<]*<\/span>//g' "$file" | grep -nE '—|&mdash;' | sed "s|^|$file:|"
done || true)
if [ -n "$dashes" ]; then
    fail "an em dash in the site's copy:"$'\n'"$dashes"
fi

exit "$failed"
