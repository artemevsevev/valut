#!/bin/bash

version="0.0.1"

docker build --tag valut:$version --platform="linux/amd64" --file Dockerfile .
docker save valut:$version > ../valut-$version.tar
