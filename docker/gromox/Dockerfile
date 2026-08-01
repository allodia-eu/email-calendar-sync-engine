# Lean Gromox harness for the MAPI/HTTP spike.
#
# NOT the official grommunio stack: that image runs 18 supervisord services
# (chat, keycloak, web, antispam, postfix, nginx, redis, ...). The spike needs
# exactly one protocol endpoint -- gromox-http, which serves the mh_emsmdb
# (MAPI/HTTP) and oxdisco (Autodiscover) handlers -- plus enough of
# grommunio-admin-api to provision one domain and one mailbox.
FROM opensuse/leap:16.0

ARG GROMMUNIO_REPO=openSUSE_Leap_16.0

RUN zypper -n install -y curl procps iproute2 \
 && curl -fsSL https://download.grommunio.com/RPM-GPG-KEY-grommunio > /tmp/gr.key \
 && rpm --import /tmp/gr.key \
 && zypper --non-interactive --quiet ar -C \
      https://download.grommunio.com/community/${GROMMUNIO_REPO} grommunio \
 && zypper --gpg-auto-import-keys ref \
 && zypper -n refresh grommunio

# gromox = the daemons + gromox-dbop (schema) ; grommunio-admin-api = the
# `grommunio-admin` CLI used to create the domain/user/mailbox ; mysql = client
# used by the entrypoint to wait for and probe the database.
RUN zypper --non-interactive install -y gromox grommunio-admin-api mysql

COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
