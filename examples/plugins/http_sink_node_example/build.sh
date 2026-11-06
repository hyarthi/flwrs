#!/bin/sh
# Type-checks the plugin and packages it into dist/ (with the protos from
# plugin/schema/ and the runtime dependencies). Needs Node.js >= 23.6, which
# runs TypeScript directly, so there's no compile step.
set -e
cd "$(dirname "$0")"
npm install --no-fund --no-audit
npx tsc --noEmit
rm -rf dist && mkdir -p dist/pkg/proto
cp -R ../../../plugin/schema dist/pkg/proto/
cp -R manifest.json package.json plugin.ts schema dist/pkg/
chmod +x dist/pkg/plugin.ts
(cd dist/pkg && npm install --omit=dev --no-fund --no-audit --no-package-lock && zip -qr ../http-sink-node-example-0.0.1.zip .)
echo "dist/http-sink-node-example-0.0.1.zip"
