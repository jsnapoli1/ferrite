#!/usr/bin/env bash
# A real mail server for the webmail tests: Dovecot (IMAP with IDLE, MOVE, UIDPLUS,
# CONDSTORE and special-use folders; LMTP delivery; SASL for Postfix) and Postfix (SMTP that
# authenticates against Dovecot and delivers into it). Plain connections on localhost only.
#
#   sudo e2e/mail-server.sh start [DIR]    # Debian/Ubuntu: installs dovecot-imapd, dovecot-lmtpd, postfix if missing
#   sudo e2e/mail-server.sh reset [DIR]    # empty every mailbox (fresh test run)
#   sudo e2e/mail-server.sh stop [DIR]
#
# Mailboxes (domain example.test): alice@example.test / alice-mail-pw, bob@example.test /
# bob-mail-pw. Ports: IMAP $IMAP_PORT (3143), SMTP $SMTP_PORT (3587). Then, for the Rust
# tests:
#   FERRITE_TEST_IMAP=127.0.0.1:3143:alice@example.test:alice-mail-pw \
#   FERRITE_TEST_SMTP=127.0.0.1:3587 cargo test -p ferrite-server --test mail
#
# Postfix runs as the system instance (its main.cf/master.cf are edited with postconf), so
# use this in a disposable container or VM, not on a machine that already handles mail.
set -euo pipefail

cmd=${1:-start}
DIR=${2:-${MAIL_TEST_DIR:-/tmp/ferrite-mail-server}}
IMAP_PORT=${IMAP_PORT:-3143}
SMTP_PORT=${SMTP_PORT:-3587}
LMTP_PORT=${LMTP_PORT:-3024}
AUTH_PORT=${AUTH_PORT:-3012}

need_root() {
  if [ "$(id -u)" != 0 ]; then echo "run as root (it starts system services)" >&2; exit 1; fi
}

install_packages() {
  if ! command -v dovecot >/dev/null || [ ! -e /usr/lib/dovecot/lmtp ] || ! command -v postfix >/dev/null; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get install -y dovecot-imapd dovecot-lmtpd postfix >/dev/null 2>&1 || { apt-get update >/dev/null && apt-get install -y dovecot-imapd dovecot-lmtpd postfix >/dev/null; }
  fi
  id vmail >/dev/null 2>&1 || useradd --system --user-group --home-dir /nonexistent --shell /usr/sbin/nologin vmail
}

write_dovecot() {
  mkdir -p "$DIR/run" "$DIR/state" "$DIR/mail"
  chown vmail:vmail "$DIR/mail"
  cat >"$DIR/users" <<EOF
alice@example.test:{PLAIN}alice-mail-pw::::::
bob@example.test:{PLAIN}bob-mail-pw::::::
EOF
  chmod 640 "$DIR/users"
  chgrp dovecot "$DIR/users"
  cat >"$DIR/dovecot.conf" <<EOF
base_dir = $DIR/run/
state_dir = $DIR/state
log_path = $DIR/dovecot.log
protocols = imap lmtp
listen = 127.0.0.1
ssl = no
disable_plaintext_auth = no
auth_mechanisms = plain login
mail_location = maildir:$DIR/mail/%d/%n/Maildir
mail_uid = vmail
mail_gid = vmail
first_valid_uid = 100
default_login_user = dovenull
default_internal_user = dovecot
mail_plugins = \$mail_plugins
passdb {
  driver = passwd-file
  args = scheme=PLAIN username_format=%u $DIR/users
}
userdb {
  driver = static
  args = uid=vmail gid=vmail home=$DIR/mail/%d/%n allow_all_users=yes
}
namespace inbox {
  inbox = yes
  separator = /
  mailbox Drafts {
    special_use = \Drafts
    auto = subscribe
  }
  mailbox Sent {
    special_use = \Sent
    auto = subscribe
  }
  mailbox Trash {
    special_use = \Trash
    auto = subscribe
  }
  mailbox Junk {
    special_use = \Junk
    auto = subscribe
  }
}
service imap-login {
  inet_listener imap {
    address = 127.0.0.1
    port = $IMAP_PORT
  }
  inet_listener imaps {
    port = 0
  }
}
service lmtp {
  inet_listener lmtp {
    address = 127.0.0.1
    port = $LMTP_PORT
  }
}
service auth {
  inet_listener {
    address = 127.0.0.1
    port = $AUTH_PORT
  }
}
protocol imap {
  mail_max_userip_connections = 100
}
EOF
}

configure_postfix() {
  postconf -e \
    "compatibility_level = 3.6" \
    "myhostname = mail.example.test" \
    "mydestination =" \
    "inet_interfaces = 127.0.0.1" \
    "inet_protocols = ipv4" \
    "mynetworks = 127.0.0.0/8" \
    "virtual_mailbox_domains = example.test" \
    "virtual_transport = lmtp:inet:127.0.0.1:$LMTP_PORT" \
    "smtpd_reject_unlisted_recipient = no" \
    "smtpd_sasl_auth_enable = yes" \
    "smtpd_sasl_type = dovecot" \
    "smtpd_sasl_path = inet:127.0.0.1:$AUTH_PORT" \
    "smtpd_tls_security_level = none" \
    "smtpd_tls_auth_only = no" \
    "smtp_tls_security_level = none" \
    "smtpd_relay_restrictions = permit_sasl_authenticated, permit_mynetworks, reject_unauth_destination" \
    "maillog_file_prefixes = /var, /dev/stdout, $DIR" \
    "maillog_file = $DIR/postfix.log" \
    "message_size_limit = 52428800"
  postconf -MX smtp/inet 2>/dev/null || true
  postconf -M "127.0.0.1:$SMTP_PORT/inet=127.0.0.1:$SMTP_PORT inet n - n - - smtpd"
}

wait_port() {
  for _ in $(seq 1 50); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; then return 0; fi
    sleep 0.1
  done
  echo "port $1 did not open" >&2
  return 1
}

stop_all() {
  if [ -f "$DIR/dovecot.conf" ]; then dovecot -c "$DIR/dovecot.conf" stop 2>/dev/null || true; fi
  postfix stop 2>/dev/null || true
}

case "$cmd" in
  start)
    need_root
    install_packages
    stop_all
    sleep 0.5
    write_dovecot
    configure_postfix
    # Daemons must not hold this script's stdout (callers may pipe it).
    dovecot -c "$DIR/dovecot.conf" </dev/null >/dev/null 2>&1
    postfix start </dev/null >/dev/null 2>&1 || postfix reload </dev/null >/dev/null 2>&1
    wait_port "$IMAP_PORT"
    wait_port "$SMTP_PORT"
    echo "$(dovecot --version | cut -d' ' -f1) IMAP on 127.0.0.1:$IMAP_PORT, Postfix $(postconf -h mail_version) SMTP on 127.0.0.1:$SMTP_PORT, data in $DIR"
    ;;
  reset)
    need_root
    rm -rf "${DIR:?}/mail"/*
    if [ -f "$DIR/dovecot.conf" ]; then doveadm -c "$DIR/dovecot.conf" kick '*' >/dev/null 2>&1 || true; fi
    echo "mailboxes emptied"
    ;;
  stop)
    need_root
    stop_all
    echo "stopped"
    ;;
  *)
    echo "usage: $0 start|reset|stop [DIR]" >&2
    exit 2
    ;;
esac
