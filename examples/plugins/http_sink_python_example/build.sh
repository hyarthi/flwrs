#!/bin/sh
# Generates the protobuf code into gen/ and packages the plugin into dist/.
# Needs: pip install grpcio-tools (bundles protoc). At run time the host's
# python3 needs `protobuf` (requirements.txt).
set -e
cd "$(dirname "$0")"
rm -rf gen dist && mkdir -p gen dist
python3 -m grpc_tools.protoc -I ../../../plugin --python_out=gen \
  schema/schema.proto schema/common.proto schema/sink.proto
chmod +x plugin.py
zip -qr dist/http-sink-python-example-0.0.1.zip manifest.json plugin.py schema gen
echo "dist/http-sink-python-example-0.0.1.zip"
