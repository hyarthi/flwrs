#!/bin/sh
# Builds the jar (generating the protobuf code from plugin/schema/) and
# packages the plugin into dist/. Needs: a JDK (21+) and Maven.
set -e
cd "$(dirname "$0")"
mvn -q package
rm -rf dist && mkdir -p dist/pkg
cp -R manifest.json bin schema target/http-sink-java-example.jar dist/pkg/
(cd dist/pkg && zip -qr ../http-sink-java-example-0.0.1.zip .)
echo "dist/http-sink-java-example-0.0.1.zip"
