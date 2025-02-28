#!/bin/bash
# Read architectures from the "archs" file
ARCHS=$(cat archs)
MANIFEST="manifest.txt"

# Create a directory for the compiled binaries
mkdir -p dist
rm dist/*

# Iterate over each architecture
for ARCH in $ARCHS; do
  echo "Compiling for architecture: $ARCH"

  # Compile the project with charge
  if ! cargo build --release --target "$ARCH"; then
    echo "Error compiling for architecture: $ARCH"
    exit 1
  fi
  echo "Successful compilation for: $ARCH"

  # Move the compiled binary to the dist directory
  BINARY_NAME="mostrod"
  if [ $ARCH == "x86_64-pc-windows-gnu" ]; then
      BINARY_NAME=$BINARY_NAME".exe"
  fi
  TARGET_DIR="target/$ARCH/release/$BINARY_NAME"
  if [ -f "$TARGET_DIR" ]; then
    cp "$TARGET_DIR" "dist/$ARCH-$BINARY_NAME"
    echo "Binary copied to: dist/$ARCH-$BINARY_NAME"
    sha256sum "dist/$ARCH-$BINARY_NAME" >> "dist/$MANIFEST"
  else
    echo "Binary for architecture not found: $ARCH"
  fi

done

echo "Chao pescao!"