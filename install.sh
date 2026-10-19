#!/bin/sh
# canvas-cloud installer — one command on a fresh Linux VM:
#
#   curl -fsSL https://raw.githubusercontent.com/copper-browser/canvas-cloud/main/install.sh | sudo sh
#   # or, from an extracted release tarball:
#   sudo ./install.sh
#
# Supported: Ubuntu 22.04/24.04, Debian 12, Amazon Linux 2023, Fedora (apt or dnf).
# Idempotent: re-running keeps the master key, config, certificate and database; it upgrades
# the binary, re-applies migrations and restarts the service.
#
# PostgreSQL is the distribution's native package (never a container), unless DATABASE_URL
# points at an existing server.
#
# Inputs (environment, all optional; they shape the config of a fresh install only):
#   DATABASE_URL                use this Postgres instead of installing one locally
#   CANVAS_CLOUD_PUBLIC_HOST    host/IP clients connect to (default: detected public IP)
#   CANVAS_CLOUD_PORT           port to listen on, all interfaces (default 443)
#   CANVAS_CLOUD_LISTEN         full listen address ip:port, e.g. 127.0.0.1:8480 (overrides
#                               CANVAS_CLOUD_PORT)
#   CANVAS_CLOUD_TLS_MODE       off | self-signed | acme (default: acme when CANVAS_CLOUD_DOMAIN
#                               is set, else self-signed); off = plain HTTP behind a TLS proxy
#   CANVAS_CLOUD_DOMAIN         public domain; with the default TLS mode it turns on Let's
#                               Encrypt (ACME) for it
#   CANVAS_CLOUD_ACME_EMAIL     contact email for ACME
#   CANVAS_CLOUD_PUBLIC_URL     base URL clients use, e.g. https://canvas.example.com
#                               (default: derived from the host, port and TLS mode)
#   CANVAS_CLOUD_MASTER_KEY     master key, base64url 32 bytes (default: generated)
#   CANVAS_CLOUD_BINARY         local path to the canvas-cloud binary or release .tar.gz
#   CANVAS_CLOUD_BINARY_URL     URL of the binary or release .tar.gz
#   CANVAS_CLOUD_VERSION        GitHub release to download (default: latest), e.g. 0.1.0
#   GITHUB_TOKEN                token for downloading from a private GitHub repository
#
# Flags: --uninstall (remove service + binary, keep data), --purge (also delete config,
#        state and the local canvas_cloud database), --dry-run (print the settings a
#        fresh install would use and change nothing; no root needed), --print-unit, --help.

set -eu

REPO="copper-browser/canvas-cloud"
BIN=/usr/local/bin/canvas-cloud
ETC=/etc/canvas-cloud
CONFIG=$ETC/canvas-cloud.toml
STATE_DIR=/var/lib/canvas-cloud
UNIT=/etc/systemd/system/canvas-cloud.service
SVC_USER=canvas-cloud
DB_NAME=canvas_cloud
DB_ROLE=canvas_cloud

log() { printf '==> %s\n' "$*" >&2; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

print_unit() {
	cat <<'UNIT_EOF'
[Unit]
Description=Canvas Cloud (real-time collaborative canvas server)
Documentation=https://github.com/copper-browser/canvas-cloud
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
Type=simple
User=canvas-cloud
Group=canvas-cloud
Environment=CANVAS_CLOUD_CONFIG=/etc/canvas-cloud/canvas-cloud.toml
ExecStart=/usr/local/bin/canvas-cloud serve
Restart=always
RestartSec=2
KillSignal=SIGTERM
TimeoutStopSec=20
LimitNOFILE=65536

# Bind :443 without root.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

# Sandboxing. The config is read-only; the certificate and ACME state live in
# /var/lib/canvas-cloud (StateDirectory). A web_root outside /home stays readable.
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
PrivateDevices=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectKernelLogs=true
ProtectControlGroups=true
ProtectClock=true
ProtectHostname=true
ProtectProc=invisible
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
RestrictNamespaces=true
RestrictRealtime=true
RestrictSUIDSGID=true
LockPersonality=true
MemoryDenyWriteExecute=true
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallFilter=~@privileged
StateDirectory=canvas-cloud
StateDirectoryMode=0750
ReadOnlyPaths=/etc/canvas-cloud
UMask=0077

[Install]
WantedBy=multi-user.target
UNIT_EOF
}

usage() {
	# The comment block at the top of this file (empty when piped into sh).
	awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "$0" 2>/dev/null || true
}

# Run canvas-cloud against the installed config with a clean environment, so installer
# inputs (some double as CANVAS_CLOUD_* config overrides) never shadow the config file.
cc() { env -i PATH="$PATH" HOME=/root LANG=C.UTF-8 "$BIN" --config "$CONFIG" "$@"; }

# psql as the postgres superuser over the local socket (never prompts).
pg() { (cd /tmp && runuser -u postgres -- psql -X -w -v ON_ERROR_STOP=1 -qtA "$@"); }

random_alnum() { LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c "$1"; }

# ----------------------------------------------------------------------------------------
# Platform

PKG=""
detect_platform() {
	[ "$(id -u)" -eq 0 ] || die "run as root (sudo sh install.sh)"
	have systemctl || die "systemd is required"
	[ -r /etc/os-release ] || die "cannot detect the distribution (/etc/os-release missing)"
	# shellcheck disable=SC1091
	. /etc/os-release
	case " ${ID:-} ${ID_LIKE:-} " in
	*" ubuntu "* | *" debian "*) PKG=apt ;;
	*" amzn "* | *" fedora "* | *" rhel "* | *" centos "*) PKG=dnf ;;
	*) die "unsupported distribution: ${PRETTY_NAME:-$ID} (need apt or dnf)" ;;
	esac
	have dnf || [ "$PKG" != dnf ] || PKG=yum
	case "$(uname -m)" in
	x86_64 | amd64) TARGET=x86_64-unknown-linux-gnu ;;
	aarch64 | arm64) TARGET=aarch64-unknown-linux-gnu ;;
	*) die "unsupported CPU architecture: $(uname -m)" ;;
	esac
	log "platform: ${PRETTY_NAME:-$ID} ($TARGET, $PKG)"
}

APT_UPDATED=0
pkg_install() {
	case "$PKG" in
	apt)
		if [ "$APT_UPDATED" -eq 0 ]; then
			DEBIAN_FRONTEND=noninteractive apt-get update -qq
			APT_UPDATED=1
		fi
		DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "$@" >/dev/null
		;;
	dnf | yum) "$PKG" install -y -q "$@" >/dev/null ;;
	esac
}

ensure_tools() {
	missing=""
	have curl || missing="$missing curl"
	have tar || missing="$missing tar"
	[ -e /etc/ssl/certs/ca-certificates.crt ] || [ -e /etc/pki/tls/certs/ca-bundle.crt ] || missing="$missing ca-certificates"
	if [ -n "$missing" ]; then
		log "installing:$missing"
		# shellcheck disable=SC2086
		pkg_install $missing
	fi
}

# ----------------------------------------------------------------------------------------
# Binary

SCRIPT_DIR=""
case "$0" in
install.sh | */install.sh) if [ -f "$0" ]; then SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd); fi ;;
esac

TMP=""
cleanup() { if [ -n "$TMP" ]; then rm -rf "$TMP"; fi; }
trap cleanup EXIT INT TERM

http_get() { # url dest [extra curl args...]
	url=$1
	dest=$2
	shift 2
	curl -fsSL --retry 3 --connect-timeout 15 --max-time 600 "$@" -o "$dest" "$url"
}

# Place a binary or release tarball ($1) at $BIN atomically.
install_artifact() {
	src=$1
	case "$src" in
	*.tar.gz | *.tgz)
		mkdir -p "$TMP/x"
		tar -xzf "$src" -C "$TMP/x"
		found=$(find "$TMP/x" -type f -name canvas-cloud | head -n 1)
		[ -n "$found" ] || die "no canvas-cloud binary inside $src"
		src=$found
		;;
	esac
	"$src" version >/dev/null 2>&1 || die "$src is not a runnable canvas-cloud binary for this machine"
	install -m 0755 "$src" "$BIN.new"
	mv -f "$BIN.new" "$BIN"
	log "installed $("$BIN" version) at $BIN"
}

# Asset API URL for asset $2 in release JSON $1 (the "url" precedes "name" per asset).
asset_api_url() {
	awk -v name="$2" '
		/"url": *"https:\/\/api\.github\.com\/repos\/.*\/releases\/assets\// { u = $0; sub(/.*"url": *"/, "", u); sub(/".*/, "", u) }
		index($0, "\"name\": \"" name "\"") { print u; exit }
	' "$1"
}

github_download() {
	ver=${CANVAS_CLOUD_VERSION:-latest}
	if [ "$ver" = latest ]; then
		api="https://api.github.com/repos/$REPO/releases/latest"
	else
		api="https://api.github.com/repos/$REPO/releases/tags/v${ver#v}"
	fi
	if [ -n "${GITHUB_TOKEN:-}" ]; then
		http_get "$api" "$TMP/release.json" -H "Authorization: Bearer $GITHUB_TOKEN" \
			-H "Accept: application/vnd.github+json"
	else
		http_get "$api" "$TMP/release.json" -H "Accept: application/vnd.github+json" ||
			die "could not query $api (private repository? set GITHUB_TOKEN, or use CANVAS_CLOUD_BINARY)"
	fi
	tag=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$TMP/release.json" | head -n 1)
	[ -n "$tag" ] || die "could not resolve release $ver of $REPO"
	asset="canvas-cloud-${tag#v}-$TARGET.tar.gz"
	log "downloading $asset ($tag)"
	if [ -n "${GITHUB_TOKEN:-}" ]; then
		url=$(asset_api_url "$TMP/release.json" "$asset")
		sums_url=$(asset_api_url "$TMP/release.json" SHA256SUMS)
		[ -n "$url" ] || die "release $tag has no asset $asset"
		http_get "$url" "$TMP/$asset" -H "Authorization: Bearer $GITHUB_TOKEN" -H "Accept: application/octet-stream"
		if [ -n "$sums_url" ]; then
			http_get "$sums_url" "$TMP/SHA256SUMS" -H "Authorization: Bearer $GITHUB_TOKEN" \
				-H "Accept: application/octet-stream" || true
		fi
	else
		base="https://github.com/$REPO/releases/download/$tag"
		http_get "$base/$asset" "$TMP/$asset" || die "release $tag has no asset $asset"
		http_get "$base/SHA256SUMS" "$TMP/SHA256SUMS" 2>/dev/null || true
	fi
	want=""
	[ ! -s "$TMP/SHA256SUMS" ] || want=$(awk -v a="$asset" '$2 == a || $2 == "*" a { print $1 }' "$TMP/SHA256SUMS")
	if [ -n "$want" ]; then
		got=$(sha256sum "$TMP/$asset" | awk '{print $1}')
		[ "$want" = "$got" ] || die "checksum mismatch for $asset"
		log "checksum ok"
	else
		warn "no checksum published for $asset"
	fi
	install_artifact "$TMP/$asset"
}

install_binary() {
	TMP=$(mktemp -d)
	if [ -n "${CANVAS_CLOUD_BINARY:-}" ]; then
		[ -f "$CANVAS_CLOUD_BINARY" ] || die "CANVAS_CLOUD_BINARY=$CANVAS_CLOUD_BINARY does not exist"
		install_artifact "$CANVAS_CLOUD_BINARY"
	elif [ -n "$SCRIPT_DIR" ] && [ -x "$SCRIPT_DIR/canvas-cloud" ] && [ ! -d "$SCRIPT_DIR/canvas-cloud" ]; then
		install_artifact "$SCRIPT_DIR/canvas-cloud"
	elif [ -n "${CANVAS_CLOUD_BINARY_URL:-}" ]; then
		case "$CANVAS_CLOUD_BINARY_URL" in
		*.tar.gz | *.tgz) dest="$TMP/canvas-cloud.tar.gz" ;;
		*) dest="$TMP/canvas-cloud" ;;
		esac
		log "downloading $CANVAS_CLOUD_BINARY_URL"
		http_get "$CANVAS_CLOUD_BINARY_URL" "$dest"
		chmod +x "$dest"
		install_artifact "$dest"
	else
		github_download
	fi
}

# ----------------------------------------------------------------------------------------
# PostgreSQL (native, local — only when DATABASE_URL is not given)

pg_service() {
	for s in postgresql postgresql-16 postgresql-15; do
		if systemctl list-unit-files "$s.service" >/dev/null 2>&1 &&
			systemctl list-unit-files "$s.service" | grep -q "^$s.service"; then
			echo "$s"
			return
		fi
	done
	echo postgresql
}

install_postgres() {
	if ! have psql || ! { have pg_ctlcluster || have postgresql-setup || [ -x /usr/bin/postgres ]; }; then
		log "installing PostgreSQL"
		case "$PKG" in
		apt) pkg_install postgresql ;;
		dnf | yum)
			pkg_install postgresql16-server 2>/dev/null ||
				pkg_install postgresql15-server 2>/dev/null ||
				pkg_install postgresql-server
			;;
		esac
	fi
	if [ "$PKG" != apt ] && have postgresql-setup; then
		# shellcheck disable=SC2016 # expanded by the postgres user's shell
		data=$(runuser -u postgres -- sh -c 'echo ${PGDATA:-/var/lib/pgsql/data}' 2>/dev/null || echo /var/lib/pgsql/data)
		if [ ! -s "$data/PG_VERSION" ]; then
			log "initializing PostgreSQL data directory"
			postgresql-setup --initdb >/dev/null
		fi
	fi
	svc=$(pg_service)
	systemctl enable --now "$svc" >/dev/null 2>&1 || systemctl start "$svc"
	i=0
	until pg -c 'SELECT 1' >/dev/null 2>&1; do
		i=$((i + 1))
		[ "$i" -lt 30 ] || die "PostgreSQL did not come up (systemctl status $svc)"
		sleep 1
	done
}

configure_pg_hba() {
	hba=$(pg -c 'SHOW hba_file')
	[ -f "$hba" ] || die "cannot find pg_hba.conf"
	if ! grep -q 'canvas-cloud (install.sh)' "$hba"; then
		log "allowing $DB_ROLE password logins on 127.0.0.1/::1 ($hba)"
		{
			echo "# canvas-cloud (install.sh): local TCP only, SCRAM passwords"
			echo "host    $DB_NAME    $DB_ROLE    127.0.0.1/32    scram-sha-256"
			echo "host    $DB_NAME    $DB_ROLE    ::1/128         scram-sha-256"
			cat "$hba"
		} >"$hba.canvas-cloud.tmp"
		cat "$hba.canvas-cloud.tmp" >"$hba"
		rm -f "$hba.canvas-cloud.tmp"
		pg -c 'SELECT pg_reload_conf()' >/dev/null
	fi
}

# Creates role + database; prints the DATABASE_URL.
setup_local_database() {
	install_postgres
	configure_pg_hba
	pw=$(random_alnum 40)
	verb=CREATE
	[ "$(pg -c "SELECT 1 FROM pg_roles WHERE rolname = '$DB_ROLE'")" = 1 ] && verb=ALTER
	# Password goes through stdin, never argv.
	printf "SET password_encryption = 'scram-sha-256';\n%s ROLE %s LOGIN PASSWORD '%s';\n" \
		"$verb" "$DB_ROLE" "$pw" | pg >/dev/null
	if [ "$(pg -c "SELECT 1 FROM pg_database WHERE datname = '$DB_NAME'")" != 1 ]; then
		pg -c "CREATE DATABASE $DB_NAME OWNER $DB_ROLE" >/dev/null
	fi
	port=$(pg -c 'SHOW port')
	log "database $DB_NAME ready (role $DB_ROLE)"
	printf 'postgres://%s:%s@127.0.0.1:%s/%s' "$DB_ROLE" "$pw" "${port:-5432}" "$DB_NAME"
}

# ----------------------------------------------------------------------------------------
# Config, TLS, service

detect_public_host() {
	if [ -n "${CANVAS_CLOUD_PUBLIC_HOST:-}" ]; then
		echo "$CANVAS_CLOUD_PUBLIC_HOST"
		return
	fi
	if [ -n "${CANVAS_CLOUD_DOMAIN:-}" ]; then
		echo "$CANVAS_CLOUD_DOMAIN"
		return
	fi
	# EC2 (IMDSv2).
	tok=$(curl -fsS -m 2 -X PUT http://169.254.169.254/latest/api/token \
		-H 'X-aws-ec2-metadata-token-ttl-seconds: 60' 2>/dev/null || true)
	if [ -n "$tok" ]; then
		ip=$(curl -fsS -m 2 -H "X-aws-ec2-metadata-token: $tok" \
			http://169.254.169.254/latest/meta-data/public-ipv4 2>/dev/null || true)
		case "$ip" in *.*.*.*) echo "$ip" && return ;; esac
	fi
	for u in https://api.ipify.org https://ifconfig.me/ip https://icanhazip.com; do
		ip=$(curl -fsS -m 5 "$u" 2>/dev/null | tr -d '[:space:]' || true)
		case "$ip" in *.*.*.* | *:*:*) echo "$ip" && return ;; esac
	done
	ip=$(hostname -I 2>/dev/null | awk '{print $1}')
	[ -n "$ip" ] || die "cannot detect the public IP; set CANVAS_CLOUD_PUBLIC_HOST"
	warn "using local address $ip (set CANVAS_CLOUD_PUBLIC_HOST if clients connect differently)"
	echo "$ip"
}

ensure_user() {
	if ! id "$SVC_USER" >/dev/null 2>&1; then
		nologin=/usr/sbin/nologin
		[ -x "$nologin" ] || nologin=/sbin/nologin
		useradd --system --user-group --home-dir "$STATE_DIR" --no-create-home \
			--shell "$nologin" --comment "canvas-cloud server" "$SVC_USER"
		log "created system user $SVC_USER"
	fi
	install -d -m 0750 -o "$SVC_USER" -g "$SVC_USER" "$STATE_DIR"
	install -d -m 0750 -o root -g "$SVC_USER" "$ETC"
}

# Bracket an IPv6 literal for use in a URL.
url_host() { case "$1" in *:*) echo "[$1]" ;; *) echo "$1" ;; esac; }

# Resolve the settings of a fresh install from the inputs into LISTEN, LISTEN_HOST, PORT,
# LOOPBACK (1/0), TLS_MODE, DOMAIN_ARG (passed as --domain; empty = none) and PUBLIC_URL.
resolve_settings() {
	PORT=${CANVAS_CLOUD_PORT:-443}
	LISTEN=${CANVAS_CLOUD_LISTEN:-}
	if [ -n "$LISTEN" ]; then
		case "$LISTEN" in
		*:*) ;;
		*) die "CANVAS_CLOUD_LISTEN must be ip:port (e.g. 127.0.0.1:8480), got $LISTEN" ;;
		esac
		PORT=${LISTEN##*:}
		if [ -n "${CANVAS_CLOUD_PORT:-}" ] && [ "$CANVAS_CLOUD_PORT" != "$PORT" ]; then
			warn "CANVAS_CLOUD_PORT=$CANVAS_CLOUD_PORT ignored: CANVAS_CLOUD_LISTEN=$LISTEN sets the port"
		fi
	else
		LISTEN="0.0.0.0:$PORT"
	fi
	case "$PORT" in
	'' | *[!0-9]*) die "invalid port in listen address $LISTEN" ;;
	esac
	LISTEN_HOST=${LISTEN%:*}
	case "$LISTEN_HOST" in
	\[*\]) ;;
	'' | *[!0-9.]*) die "the listen host must be an IP address (e.g. 127.0.0.1:8480 or [::1]:8480), got $LISTEN" ;;
	esac
	LOOPBACK=0
	case "$LISTEN_HOST" in 127.* | "[::1]") LOOPBACK=1 ;; esac

	TLS_MODE=${CANVAS_CLOUD_TLS_MODE:-}
	case "$TLS_MODE" in
	"") if [ -n "${CANVAS_CLOUD_DOMAIN:-}" ]; then TLS_MODE=acme; else TLS_MODE=self-signed; fi ;;
	off | acme | self-signed) ;;
	selfsigned | self_signed) TLS_MODE=self-signed ;;
	*) die "CANVAS_CLOUD_TLS_MODE must be off, self-signed or acme (got $TLS_MODE)" ;;
	esac
	DOMAIN_ARG=""
	case "$TLS_MODE" in
	acme)
		[ -n "${CANVAS_CLOUD_DOMAIN:-}" ] || die "CANVAS_CLOUD_TLS_MODE=acme needs CANVAS_CLOUD_DOMAIN"
		[ "$PORT" = 443 ] || warn "ACME (TLS-ALPN-01) needs this server reachable on port 443 from the internet"
		[ "$LOOPBACK" = 0 ] || warn "ACME cannot complete on a loopback-only listener ($LISTEN)"
		DOMAIN_ARG=$CANVAS_CLOUD_DOMAIN
		;;
	self-signed) DOMAIN_ARG=${CANVAS_CLOUD_DOMAIN:-} ;;
	off)
		[ "$LOOPBACK" = 1 ] ||
			warn "TLS is off on $LISTEN (not loopback): plain HTTP; keep it firewalled behind your TLS proxy"
		;;
	esac

	if [ -n "${CANVAS_CLOUD_PUBLIC_URL:-}" ]; then
		PUBLIC_URL=$CANVAS_CLOUD_PUBLIC_URL
	elif [ "$TLS_MODE" = off ]; then
		if [ -n "${CANVAS_CLOUD_PUBLIC_HOST:-}${CANVAS_CLOUD_DOMAIN:-}" ]; then
			# A named host with TLS off: clients reach it through a TLS proxy on 443.
			host=$(detect_public_host)
			PUBLIC_URL="https://$(url_host "$host")"
		elif [ "$LOOPBACK" = 1 ]; then
			# Loopback only: the clients are on this machine (a proxy or a linking service).
			PUBLIC_URL="http://$LISTEN"
		else
			host=$(detect_public_host)
			PUBLIC_URL="http://$(url_host "$host"):$PORT"
		fi
	else
		host=$(detect_public_host)
		if [ "$PORT" = 443 ]; then
			PUBLIC_URL="https://$(url_host "$host")"
		else
			PUBLIC_URL="https://$(url_host "$host"):$PORT"
		fi
	fi
}

# Warn about inputs that a re-run (existing config) does not apply.
warn_ignored_inputs() {
	for v in CANVAS_CLOUD_PORT CANVAS_CLOUD_LISTEN CANVAS_CLOUD_TLS_MODE CANVAS_CLOUD_DOMAIN \
		CANVAS_CLOUD_ACME_EMAIL CANVAS_CLOUD_PUBLIC_HOST CANVAS_CLOUD_PUBLIC_URL CANVAS_CLOUD_MASTER_KEY DATABASE_URL; do
		eval "val=\${$v:-}"
		# shellcheck disable=SC2154 # set by eval
		[ -z "$val" ] || warn "$v ignored: $CONFIG exists (edit it, then systemctl restart canvas-cloud)"
	done
}

write_config() {
	if [ -f "$CONFIG" ]; then
		log "keeping existing config $CONFIG (master key and database unchanged)"
		warn_ignored_inputs
		return
	fi
	resolve_settings
	set -- --tls-mode "$TLS_MODE"
	[ -z "$DOMAIN_ARG" ] || set -- "$@" --domain "$DOMAIN_ARG"
	if [ "$TLS_MODE" = acme ] && [ -n "${CANVAS_CLOUD_ACME_EMAIL:-}" ]; then
		set -- "$@" --acme-email "$CANVAS_CLOUD_ACME_EMAIL"
	fi
	if [ -n "${DATABASE_URL:-}" ]; then
		db_url=$DATABASE_URL
		log "using external database from DATABASE_URL"
	else
		db_url=$(setup_local_database)
	fi
	log "writing $CONFIG (listen $LISTEN, public url $PUBLIC_URL, tls $TLS_MODE)"
	# Secrets travel through the environment, never argv.
	env -i PATH="$PATH" HOME=/root \
		CANVAS_CLOUD_DATABASE_URL="$db_url" \
		CANVAS_CLOUD_MASTER_KEY="${CANVAS_CLOUD_MASTER_KEY:-}" \
		"$BIN" gen-config --write "$CONFIG" \
		--public-url "$PUBLIC_URL" \
		--listen "$LISTEN" \
		--state-dir "$STATE_DIR" \
		"$@" >/dev/null
}

# --dry-run: what a fresh install would configure. Changes nothing, needs no root.
dry_run() {
	if [ -f "$CONFIG" ]; then
		echo "$CONFIG exists: an install would keep it unchanged and only upgrade the binary."
		echo
	fi
	resolve_settings
	if [ -n "${DATABASE_URL:-}" ]; then
		db="DATABASE_URL (external; nothing installed)"
	else
		db="local native PostgreSQL: role $DB_ROLE, database $DB_NAME, password generated"
	fi
	mk=generated
	[ -z "${CANVAS_CLOUD_MASTER_KEY:-}" ] || mk="from CANVAS_CLOUD_MASTER_KEY"
	if [ "$LOOPBACK" = 1 ]; then
		fw="not opened (loopback only)"
	elif [ "$TLS_MODE" = off ]; then
		fw="not opened (plain HTTP; open it yourself if a proxy on another host connects)"
	else
		fw="open $PORT/tcp (ufw or firewalld, when active)"
	fi
	cat <<EOF
listen       $LISTEN
tls mode     $TLS_MODE
public url   $PUBLIC_URL
database     $db
master key   $mk
config       $CONFIG (0600, owner $SVC_USER)
state        $STATE_DIR
firewall     $fw

The config would be written with:
  canvas-cloud gen-config --write $CONFIG --public-url $PUBLIC_URL --listen $LISTEN \\
    --state-dir $STATE_DIR --tls-mode $TLS_MODE${DOMAIN_ARG:+ --domain $DOMAIN_ARG}
EOF
}

secure_files() {
	chown "$SVC_USER:$SVC_USER" "$CONFIG"
	chmod 0600 "$CONFIG"
	chown -R "$SVC_USER:$SVC_USER" "$STATE_DIR"
	[ ! -f "$STATE_DIR/tls/key.pem" ] || chmod 0600 "$STATE_DIR/tls/key.pem"
}

open_firewall() {
	port=$(sed -n 's/^listen *= *"[^"]*:\([0-9]*\)".*/\1/p' "$CONFIG" | head -n 1)
	port=${port:-443}
	if grep -Eq '^listen *= *"(127\.|\[::1\])' "$CONFIG"; then
		log "listening on loopback only: firewall unchanged"
		return
	fi
	if grep -q '^mode = "off"' "$CONFIG"; then
		log "TLS is off: not opening $port/tcp in the firewall (the TLS proxy in front is what clients reach)"
		return
	fi
	if have ufw && ufw status 2>/dev/null | grep -q '^Status: active'; then
		ufw allow "$port/tcp" >/dev/null && log "ufw: allowed $port/tcp"
	fi
	if have firewall-cmd && firewall-cmd --state >/dev/null 2>&1; then
		firewall-cmd --quiet --permanent --add-port="$port/tcp" && firewall-cmd --quiet --reload &&
			log "firewalld: allowed $port/tcp"
	fi
}

install_service() {
	tmp_unit=$(mktemp)
	if [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/packaging/canvas-cloud.service" ]; then
		cp "$SCRIPT_DIR/packaging/canvas-cloud.service" "$tmp_unit"
	else
		print_unit >"$tmp_unit"
	fi
	install -m 0644 "$tmp_unit" "$UNIT"
	rm -f "$tmp_unit"
	systemctl daemon-reload
	systemctl enable canvas-cloud.service >/dev/null 2>&1
}

# public_url from the config.
config_public_url() { sed -n 's/^public_url *= *"\([^"]*\)".*/\1/p' "$CONFIG" | head -n 1; }

uninstall() {
	purge=$1
	detect_platform
	log "stopping and removing the service"
	systemctl disable --now canvas-cloud.service >/dev/null 2>&1 || true
	rm -f "$UNIT"
	systemctl daemon-reload
	rm -f "$BIN"
	if [ "$purge" = 1 ]; then
		if have psql && id postgres >/dev/null 2>&1 && pg -c 'SELECT 1' >/dev/null 2>&1; then
			if [ "$(pg -c "SELECT 1 FROM pg_database WHERE datname = '$DB_NAME'")" = 1 ]; then
				pg -c "DROP DATABASE $DB_NAME" >/dev/null && log "dropped local database $DB_NAME"
			fi
			if [ "$(pg -c "SELECT 1 FROM pg_roles WHERE rolname = '$DB_ROLE'")" = 1 ]; then
				pg -c "DROP ROLE $DB_ROLE" >/dev/null && log "dropped role $DB_ROLE"
			fi
		fi
		rm -rf "$ETC" "$STATE_DIR"
		userdel "$SVC_USER" >/dev/null 2>&1 || true
		log "purged config, master key, certificate and state"
	else
		log "kept $ETC (config + master key) and the database; use --purge to delete them"
	fi
}

main() {
	case "${1:-}" in
	--help | -h)
		usage
		exit 0
		;;
	--print-unit)
		print_unit
		exit 0
		;;
	--dry-run)
		dry_run
		exit 0
		;;
	--uninstall)
		uninstall 0
		exit 0
		;;
	--purge)
		uninstall 1
		exit 0
		;;
	"") ;;
	*) die "unknown option: $1 (see --help)" ;;
	esac

	detect_platform
	ensure_tools
	upgrading=0
	[ -f "$CONFIG" ] && upgrading=1
	install_binary
	ensure_user
	write_config
	if [ "$upgrading" = 1 ] && [ -z "${DATABASE_URL:-}" ] && grep -q '@127.0.0.1:' "$CONFIG"; then
		svc=$(pg_service)
		systemctl start "$svc" 2>/dev/null || true
	fi
	cc tls-init >/dev/null
	secure_files
	log "applying database migrations"
	cc migrate
	install_service
	open_firewall
	log "starting canvas-cloud"
	systemctl restart canvas-cloud.service

	wait_secs=60
	grep -q '^mode = "acme"' "$CONFIG" && wait_secs=180
	if ! cc healthcheck --wait "$wait_secs" >/dev/null 2>&1; then
		journalctl -u canvas-cloud.service -n 30 --no-pager >&2 || true
		if grep -q '^mode = "acme"' "$CONFIG"; then
			warn "not answering yet — ACME issuance can take a few minutes (journalctl -u canvas-cloud -f)"
		else
			die "canvas-cloud did not become healthy (systemctl status canvas-cloud)"
		fi
	fi
	cc doctor >/dev/null 2>&1 || warn "canvas-cloud doctor reported problems: run 'sudo canvas-cloud --config $CONFIG doctor'"

	echo
	echo "canvas-cloud is running at $(config_public_url)"
	echo
	if grep -q '^mode = "self-signed"' "$CONFIG"; then
		echo "  (self-signed certificate: clients see a warning until you set CANVAS_CLOUD_DOMAIN or a proxy)"
	elif grep -q '^mode = "off"' "$CONFIG"; then
		echo "  (TLS is off: plain HTTP on $(sed -n 's/^listen *= *"\([^"]*\)".*/\1/p' "$CONFIG" | head -n 1);"
		echo "   put a TLS-terminating reverse proxy in front, see docs/install.md)"
	fi
	echo "Next steps:"
	echo "  create the first account: sudo canvas-cloud --config $CONFIG admin users create --email you@example.com --password-file -"
	echo "  open signup to others:     sudo canvas-cloud --config $CONFIG admin settings set allow_signup true"
	echo "  link an app:               sudo canvas-cloud --config $CONFIG apps create --name 'My app'"
	echo "  serve a web app:           set web_root in $CONFIG, then systemctl restart canvas-cloud"
	echo "Health: sudo canvas-cloud --config $CONFIG doctor"
}

main "$@"
