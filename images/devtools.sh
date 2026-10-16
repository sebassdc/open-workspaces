#!/bin/sh
# ow-image: base=ubuntu
# Example golden image: the Ubuntu developer image plus a few extra tools.
# Build it on a runner with `ow image build images/devtools.sh`, then
# `ow create box --image devtools --memory 1024`.
set -eu
apt-get update
apt-get install -y --no-install-recommends tree htop
echo "devtools $(date -u +%F)" > /etc/ow-golden
