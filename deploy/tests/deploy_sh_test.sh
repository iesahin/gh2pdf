#!/usr/bin/env bash
#
# Tests for the helpers in deploy/deploy.sh: the nginx server_name detection
# that decides whether the site file has to be regenerated before certbot
# runs, and the usage text.
#
# Usage: deploy/tests/deploy_sh_test.sh

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEPLOY_DIR="$(dirname "$TEST_DIR")"

# shellcheck source=../deploy.sh
GH2PDF_DEPLOY_LIB_ONLY=1 source "$DEPLOY_DIR/deploy.sh"
set +e

failures=0

# Runs config_has_server_name over $2 and compares the outcome with $1
# ("match" or "no-match"); $3 names the case in the report.
assert_match() {
    local expected="$1" config="$2" name="$3" actual

    if config_has_server_name "gh2pdf.emresult.com" <<<"$config"; then
        actual="match"
    else
        actual="no-match"
    fi

    if [[ "$actual" == "$expected" ]]; then
        echo "ok - $name"
    else
        echo "FAIL - $name (expected $expected, got $actual)"
        failures=$((failures + 1))
    fi
}

rendered_site() {
    sed -e "s/DOMAIN_PLACEHOLDER/$1/g" -e "s/PORT_PLACEHOLDER/8080/g" \
        "$DEPLOY_DIR/nginx-gh2pdf.conf"
}

assert_match match "$(rendered_site gh2pdf.emresult.com)" \
    "site rendered for the deployed domain matches"

# The regression: a site file left behind by a run with another --domain used
# to be kept as-is, so nginx had no block for the new domain and certbot
# failed with "Could not automatically find a matching server block".
assert_match no-match "$(rendered_site gh2pdf.example.com)" \
    "site rendered for a different domain does not match"

assert_match no-match "$(cat "$DEPLOY_DIR/nginx-gh2pdf.conf")" \
    "unrendered template does not match"

assert_match match 'server {
    server_name www.emresult.com gh2pdf.emresult.com;
}' "domain listed alongside other names matches"

assert_match no-match 'server {
    server_name gh2pdf.emresult.com.example.net;
}' "domain as a substring of another name does not match"

assert_match no-match 'server {
    server_name api.gh2pdf.emresult.com;
}' "subdomain of the domain does not match"

assert_match no-match 'server {
    # server_name gh2pdf.emresult.com;
    server_name _;
}' "commented-out server_name does not match"

assert_match match 'server {
	server_name gh2pdf.emresult.com; # tab-indented, trailing comment
}' "tab indentation and trailing comment still match"

# certbot's own installed configuration keeps the name on the TLS block.
assert_match match 'server {
    listen 443 ssl; # managed by Certbot
    server_name gh2pdf.emresult.com;
}' "certbot-managed TLS block matches"

# usage() used to strip the comment marker from $0, which made the line stop
# matching the "still in the comment block" rule, so --help printed nothing.
# It reads the running script's own header, so run deploy.sh rather than the
# sourced copy of the function.
usage_text="$(bash "$DEPLOY_DIR/deploy.sh" --help)"
if [[ "$usage_text" == *"--domain gh2pdf.example.com"* ]]; then
    echo "ok - usage prints the full header comment"
else
    echo "FAIL - usage prints the full header comment (got: ${usage_text})"
    failures=$((failures + 1))
fi

if [[ "$failures" -ne 0 ]]; then
    echo "$failures test(s) failed"
    exit 1
fi

echo "all tests passed"
