# clean project
clean:
    cargo xtask clean

# build all the project with clean before
buildall: clean
    cargo xtask build-all

# build the project
build:
    cargo xtask build


