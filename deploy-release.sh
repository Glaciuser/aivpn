#!/bin/bash

set -euo pipefail

SERVER="83.222.16.182"
USER="root"
PASSWORD='!RRp7Z4hDPxR'
REMOTE_DIR="/root/aivpn-releases/v0.4.0"

echo "=== Deploying AIVPN v0.4.0 Release to $SERVER ==="
echo ""

# Create remote directory
echo "Creating remote directory..."
sshpass -p "$PASSWORD" ssh -o StrictHostKeyChecking=no "$USER@$SERVER" "mkdir -p $REMOTE_DIR"

# Copy all release artifacts
echo "Copying release artifacts..."
ARTIFACTS=(
    "releases/aivpn-server"
    "releases/aivpn-server-linux-x86_64"
    "releases/aivpn-server-linux-arm64"
    "releases/aivpn-server-linux-armv7-musleabihf"
    "releases/aivpn-server-linux-mipsel-musl"
    "releases/aivpn-client-linux-x86_64"
    "releases/aivpn-client-linux-arm64"
    "releases/aivpn-client-linux-armv7-musleabihf"
    "releases/aivpn-client-linux-mipsel-musl"
    "releases/aivpn-client-macos-universal"
    "releases/aivpn-macos.pkg"
    "releases/aivpn-macos.dmg"
    "releases/aivpn-client.apk"
    "CHANGELOG.md"
)

for artifact in "${ARTIFACTS[@]}"; do
    if [[ -f "$artifact" ]]; then
        echo "  📦 $artifact"
        sshpass -p "$PASSWORD" scp -o StrictHostKeyChecking=no "$artifact" "$USER@$SERVER:$REMOTE_DIR/"
    else
        echo "  ⚠️  $artifact not found, skipping..."
    fi
done

echo ""
echo "=== Deployment Complete ==="
echo "Files available at: $REMOTE_DIR on $SERVER"
echo ""
echo "To download from server:"
echo "  sshpass -p '$PASSWORD' ssh -o StrictHostKeyChecking=no $USER@$SERVER \"ls -lh $REMOTE_DIR\""
