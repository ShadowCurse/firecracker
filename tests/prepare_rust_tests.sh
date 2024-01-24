#!/bin/bash

set -e

mkdir -p rust_test_results

TOKEN=`curl -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600"` 
INSTANCE=`curl -H "X-aws-ec2-metadata-token: $TOKEN" -v http://169.254.169.254/latest/meta-data/instance-type`

KERNEL=$(uname -r)

echo ${INSTANCE}/${KERNEL} > rust_test_results/instance

./tools/devtool -y test --rust --performance
