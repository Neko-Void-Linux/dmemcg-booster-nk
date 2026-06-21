#!/bin/sh
set -eu

PREFIX=${PREFIX:-/usr}
DESTDIR=${DESTDIR:-}
ENABLE=0
SERVICE_DIR=${SERVICE_DIR:-}
ACTIVE_DIR=${ACTIVE_DIR:-}
BINARY=${BINARY:-target/release/dmemcg-booster}

usage() {
    cat <<'USAGE'
Usage: ./install-runit.sh [--enable] [--service-dir DIR] [--active-dir DIR]

Builds when needed, then installs the binary, system service, documentation,
and a per-user runit service template.

Environment overrides: PREFIX, DESTDIR, SERVICE_DIR, ACTIVE_DIR, BINARY.
USAGE
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --enable) ENABLE=1 ;;
        --service-dir)
            shift
            SERVICE_DIR=${1:?--service-dir requires a value}
            ;;
        --active-dir)
            shift
            ACTIVE_DIR=${1:?--active-dir requires a value}
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

if [ "$ENABLE" -eq 1 ] && [ -n "$DESTDIR" ]; then
    echo "--enable cannot be used with DESTDIR" >&2
    exit 1
fi

if [ ! -x "$BINARY" ]; then
    command -v cargo >/dev/null 2>&1 || {
        echo "cargo is required because $BINARY does not exist" >&2
        exit 1
    }
    cargo build --release --locked
fi

if [ -z "$SERVICE_DIR" ]; then
    if [ -d /etc/runit/sv ] && [ ! -d /etc/sv ]; then
        SERVICE_DIR=/etc/runit/sv
    else
        SERVICE_DIR=/etc/sv
    fi
fi

if [ -z "$ACTIVE_DIR" ]; then
    if [ -e /run/runit/service ]; then
        ACTIVE_DIR=/run/runit/service
    elif [ -e /var/service ]; then
        ACTIVE_DIR=/var/service
    elif [ -d /etc/runit/runsvdir/default ]; then
        ACTIVE_DIR=/etc/runit/runsvdir/default
    else
        ACTIVE_DIR=/var/service
    fi
fi

install -Dm755 "$BINARY" "$DESTDIR$PREFIX/bin/dmemcg-booster"
install -Dm644 LICENSE "$DESTDIR$PREFIX/share/licenses/dmemcg-booster/LICENSE"
install -Dm644 README.md "$DESTDIR$PREFIX/share/doc/dmemcg-booster/README.md"
install -Dm644 PORTING-NOTES.md "$DESTDIR$PREFIX/share/doc/dmemcg-booster/PORTING-NOTES.md"

service="$DESTDIR$SERVICE_DIR/dmemcg-booster"
install -d "$service/log"
install -m755 runit/system/dmemcg-booster/run "$service/run"
sed "s|^DAEMON=.*|DAEMON=\"$PREFIX/bin/dmemcg-booster\"|" \
    runit/system/dmemcg-booster/conf >"$service/conf"
chmod 644 "$service/conf"
install -m755 runit/system/dmemcg-booster/log/run "$service/log/run"
install -m644 runit/system/dmemcg-booster/log/config "$service/log/config"

user_template="$DESTDIR$PREFIX/share/dmemcg-booster/runit/user/dmemcg-booster"
install -d "$user_template"
install -m755 runit/user/dmemcg-booster/run "$user_template/run"
sed "s|^DAEMON=.*|DAEMON=\"$PREFIX/bin/dmemcg-booster\"|" \
    runit/user/dmemcg-booster/conf >"$user_template/conf"
chmod 644 "$user_template/conf"

if [ "$ENABLE" -eq 1 ]; then
    mkdir -p "$ACTIVE_DIR"
    active_link="$ACTIVE_DIR/dmemcg-booster"
    expected_target="$SERVICE_DIR/dmemcg-booster"
    if [ -L "$active_link" ]; then
        actual_target=$(readlink "$active_link")
        if [ "$actual_target" != "$expected_target" ]; then
            echo "$active_link already points to $actual_target" >&2
            exit 1
        fi
    elif [ -e "$active_link" ]; then
        echo "$active_link already exists and is not a symlink" >&2
        exit 1
    else
        ln -s "$expected_target" "$active_link"
    fi
fi

echo "Installed dmemcg-booster and the system runit service in $SERVICE_DIR/dmemcg-booster"
[ "$ENABLE" -eq 1 ] && echo "Enabled it through $ACTIVE_DIR/dmemcg-booster"
echo "User-service template: $PREFIX/share/dmemcg-booster/runit/user/dmemcg-booster"
