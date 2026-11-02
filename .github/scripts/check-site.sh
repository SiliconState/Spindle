#!/usr/bin/env bash
# A few claims the site once made that the code outgrew, the download's version, and the
# site's own rules. pages.yml runs it before the site is published (only when site/ or
# this script changes); run it by hand from the repository root too. It greps the site's
# wording and reads no code, so it catches only what is listed here: copy that rewords one
# of these claims needs its pattern looked at too.
set -euo pipefail
# From the repository root, wherever it is run from.
cd "$(dirname "$0")/../.."

page=site/browser/index.html
failed=0
fail() {
    echo "check-site: $1" >&2
    failed=1
}

# Browser threads have had a spending limit since 0.1.4. Earlier copy said they "have no
# spending limit of their own", and its table's limit row said "None"; "leave it empty for
# no limit" is fine.
if grep -qiE '(has|have) no (spending|dollar|spend) limit|Spending limit</td>[[:space:]]*<td>[[:space:]]*None' "$page"; then
    fail "$page says browser threads have no spending limit; they have an optional one per thread"
fi

# The download buttons and the closing version line name one release: the tag and the zip
# in each link, and the version shown, all the same.
versions=$(grep -oE 'releases/download/browser-v[0-9]+\.[0-9]+\.[0-9]+/spindle-browser-[0-9]+\.[0-9]+\.[0-9]+\.zip|<small>[0-9]+\.[0-9]+\.[0-9]+ ·|Spindle for Browser [0-9]+\.[0-9]+\.[0-9]+' "$page" \
    | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | sort -u || true)
if [ -z "$versions" ]; then
    fail "$page names no version to download"
elif [ "$(wc -l <<<"$versions")" != 1 ]; then
    fail "$page names more than one version for its download: $(tr '\n' ' ' <<<"$versions")"
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

# No em dashes in what the site says (its section numbers use them as a design mark), as
# the character or any of its HTML references. grep reads all of it (no -q): under
# pipefail, sed cut off by an early exit would fail the pipeline and hide the match.
dashes=$(for file in site/*.html site/*/*.html; do
    sed 's/<span class="num">[^<]*<\/span>//g' "$file" | grep -niE '—|&mdash;|&#8212;|&#x2014;' | sed "s|^|$file:|"
done || true)
if [ -n "$dashes" ]; then
    fail "an em dash in the site's copy:"$'\n'"$dashes"
fi

exit "$failed"
