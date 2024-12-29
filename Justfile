# clean project
clean:
    cargo xtask clean

# build all the project with clean before
build-all: clean
    cargo xtask build-all

# build the project
build:
    cargo xtask build

# build the mostro db
build-db:
    cargo xtask build-db

# run mostro
run-mostro:
    cargo run --release --bin mostrod

# run mostro debug
runmostro-debug:
    cargo run --bin mostrod

