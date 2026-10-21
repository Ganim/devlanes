# Throwaway sshd box for the `needs ssh` tests; e2e/ssh/run.sh builds and runs it.
# bookworm matches the rust:1.99.1-bookworm image that builds the mounted devlanes binary.
FROM debian:bookworm-slim

# Password `*` disables password login without marking the account locked, which sshd refuses.
RUN apt-get update \
 && apt-get install -y --no-install-recommends openssh-server \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --create-home --shell /bin/sh devlanes \
 && usermod -p '*' devlanes \
 && install -d -m 700 -o devlanes -g devlanes /home/devlanes/.ssh \
 && mkdir -p /run/sshd \
 && ssh-keygen -A

COPY sshd_config /etc/ssh/sshd_config.d/devlanes.conf

EXPOSE 22

# The key arrives as an env var: a bind-mounted authorized_keys can have host ownership
# that sshd's StrictModes rejects.
CMD ["sh", "-c", "printf '%s\\n' \"$AUTHORIZED_KEY\" > /home/devlanes/.ssh/authorized_keys && chown devlanes:devlanes /home/devlanes/.ssh/authorized_keys && chmod 600 /home/devlanes/.ssh/authorized_keys && exec /usr/sbin/sshd -D -e"]
