#!/bin/sh
# Generates the protobuf code into pb/, builds bin/ and packages the plugin
# into dist/ (for this machine's platform; set GOOS / GOARCH to cross-build).
# Needs: protoc and protoc-gen-go (go install google.golang.org/protobuf/cmd/protoc-gen-go@latest).
set -e
cd "$(dirname "$0")"
rm -rf pb bin dist && mkdir -p pb bin dist
# The protos have no go_package option: map all three into one Go package.
M=""
for f in schema/schema.proto schema/common.proto schema/sink.proto; do
  M="$M --go_opt=M$f=flwrs.example/http-sink-go/pb"
done
protoc -I ../../../plugin --go_out=pb --go_opt=paths=source_relative $M \
  schema/schema.proto schema/common.proto schema/sink.proto
mv pb/schema/*.go pb/ && rmdir pb/schema
go mod tidy
go build -o bin/http-sink-go-example .
zip -qr dist/http-sink-go-example-0.0.1.zip manifest.json bin schema
echo "dist/http-sink-go-example-0.0.1.zip"
