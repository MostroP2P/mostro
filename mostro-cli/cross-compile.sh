#!/bin/bash
# To make this work you need to have cross installed
# cargo install cross
app="mostro-cli"
file="archs"
manifest="manifest.txt"
arch=`cat $file`
if [ ! -d bin ]; then
  mkdir bin
fi
rm bin/*
for i in $arch; do
    echo "Cross compiling for $i"
    cross build --release --target $i
    filename=$app
    if [ $i == "x86_64-pc-windows-gnu" ]; then
        filename=$filename".exe"
    fi
    cd target/$i/release
    mkdir $i
    cp $filename $i/
    sha256sum $i/$filename >> ../../../bin/$manifest
    tar -czf $app-$i.tar.gz $i
    sha256sum $app-$i.tar.gz >> ../../../bin/$manifest
    mv $app-$i.tar.gz ../../../bin
    rm -rf $i
    cd ../../../
done