#!/bin/sh

# https://gist.github.com/taoyuan/39d9bc24bafc8cc45663683eae36eb1a

# The bash in Git for Windows will replace the leading slash with git bash root folder
# and double slash can escape but the first field of CSR can't be recognized by openssl.
# Use a place holder field to work around.

openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:secp384r1 -days 65535 -nodes -x509 \
    -subj "//PLACEHOLDER=PLACEHOLDER/C=MA/L=Casablanca/O=SHIROHATO/OU=Principal Team/CN=ange.princess" \
    -keyout cert.key  -out cert.crt
