#!/bin/bash
# Test AIVPN client connection
export RUST_LOG=debug
exec ./target/release/aivpn-client \
  --server 217.26.25.6:51443 \
  --server-key '5U3zX00rZeTEqQQHxQnNTOfm+NIJQG88bgoqE0p9lmo=' \
  2>&1 | tee /tmp/aivpn-client.log
