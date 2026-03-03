#!/bin/bash
# Run mutation testing locally with helpful options
# Usage: ./scripts/mutation-test.sh [full|quick|file <path>|help]

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

print_help() {
    echo -e "${BLUE}Mostro Mutation Testing Helper${NC}"
    echo ""
    echo "Usage: $0 [command]"
    echo ""
    echo "Commands:"
    echo "  full          Run full mutation testing (slow: 30-60 min)"
    echo "  quick         Test only files changed since last commit (fast: 5-10 min)"
    echo "  file <path>   Test specific file only"
    echo "  install       Install cargo-mutants"
    echo "  clean         Clean mutation output directory"
    echo "  report        Show last mutation report"
    echo "  help          Show this help"
    echo ""
    echo "Examples:"
    echo "  $0 quick                    # Quick check on changed files"
    echo "  $0 file src/flow.rs         # Test specific module"
    echo "  $0 full                     # Complete mutation testing"
}

check_install() {
    if ! cargo mutants --version &> /dev/null; then
        echo -e "${YELLOW}cargo-mutants not found. Installing...${NC}"
        cargo install cargo-mutants
    fi
}

# Cross-platform open command
open_report() {
    local file="$1"
    if [ ! -f "$file" ]; then
        echo -e "${YELLOW}No HTML report found at $file${NC}"
        return
    fi
    if command -v xdg-open &> /dev/null; then
        xdg-open "$file"
    elif command -v open &> /dev/null; then
        open "$file"
    else
        echo -e "${BLUE}Open in your browser: $file${NC}"
    fi
}

run_full() {
    check_install
    echo -e "${BLUE}Running full mutation testing...${NC}"
    echo -e "${YELLOW}This may take 30-60 minutes depending on your hardware.${NC}"
    cargo mutants --output mutants.out
    echo -e "${GREEN}Done! Results in mutants.out/${NC}"
    if [ -f "mutants.out/html/index.html" ]; then
        echo -e "${GREEN}HTML report: mutants.out/html/index.html${NC}"
        open_report "mutants.out/html/index.html"
    fi
}

run_quick() {
    check_install
    echo -e "${BLUE}Running mutation testing on changed files only...${NC}"
    # Check if HEAD~1 exists (may not in shallow clones or initial commit)
    if git rev-parse HEAD~1 &>/dev/null; then
        # Generate a diff file for cargo-mutants --in-diff
        git diff HEAD~1 > /tmp/mostro-quick-mutants.diff
        if [ -s /tmp/mostro-quick-mutants.diff ]; then
            cargo mutants --in-diff /tmp/mostro-quick-mutants.diff --output mutants.out
        else
            echo -e "${YELLOW}No changes detected in HEAD~1. Running on all files.${NC}"
            cargo mutants --output mutants.out
        fi
        rm -f /tmp/mostro-quick-mutants.diff
    else
        echo -e "${YELLOW}Warning: HEAD~1 not available (shallow clone or initial commit).${NC}"
        echo -e "${YELLOW}Running full mutation testing instead.${NC}"
        cargo mutants --output mutants.out
    fi
}

run_file() {
    check_install
    if [ -z "$1" ]; then
        echo -e "${RED}Error: No file specified${NC}"
        echo "Usage: $0 file <path>"
        exit 1
    fi
    echo -e "${BLUE}Running mutation testing on $1...${NC}"
    cargo mutants --file "$1" --output mutants.out
    echo -e "${GREEN}Done! Results in mutants.out/${NC}"
}

run_install() {
    echo -e "${BLUE}Installing cargo-mutants...${NC}"
    cargo install cargo-mutants
    echo -e "${GREEN}Done!${NC}"
}

run_clean() {
    echo -e "${BLUE}Cleaning mutation output...${NC}"
    rm -rf mutants.out
    echo -e "${GREEN}Done!${NC}"
}

run_report() {
    if [ ! -d "mutants.out" ]; then
        echo -e "${RED}No mutation output found. Run mutation testing first.${NC}"
        exit 1
    fi

    if [ -f "mutants.out/outcomes.json" ]; then
        total=$(jq '. | length' mutants.out/outcomes.json 2>/dev/null || echo "0")
        killed=$(jq '[.[] | select(.summary == "Killed" or .summary == "CaughtMutant")] | length' mutants.out/outcomes.json 2>/dev/null || echo "0")
        survived=$(jq '[.[] | select(.summary == "Survived" or .summary == "MissedMutant")] | length' mutants.out/outcomes.json 2>/dev/null || echo "0")
        timeout=$(jq '[.[] | select(.summary == "Timeout")] | length' mutants.out/outcomes.json 2>/dev/null || echo "0")
        unviable=$(jq '[.[] | select(.summary == "Unviable")] | length' mutants.out/outcomes.json 2>/dev/null || echo "0")

        if [ "$total" -gt 0 ]; then
            score=$(echo "scale=1; ($killed / $total) * 100" | bc)

            echo -e "${BLUE}=== Mutation Testing Results ===${NC}"
            echo ""
            echo "Total Mutants: $total"
            echo -e "${GREEN}Killed: $killed${NC}"
            echo -e "${RED}Survived: $survived${NC}"
            echo -e "${YELLOW}Timeout: $timeout${NC}"
            echo "Unviable: $unviable"
            echo ""

            if (( $(echo "$score >= 80" | bc -l) )); then
                echo -e "Mutation Score: ${GREEN}$score%${NC} ✅ Excellent"
            elif (( $(echo "$score >= 50" | bc -l) )); then
                echo -e "Mutation Score: ${YELLOW}$score%${NC} ⚠️ Acceptable"
            else
                echo -e "Mutation Score: ${RED}$score%${NC} 🔴 Poor"
            fi
        fi
    fi

    if [ -f "mutants.out/html/index.html" ]; then
        echo ""
        echo -e "${BLUE}Full HTML report: mutants.out/html/index.html${NC}"
    fi
}

# Main
case "${1:-help}" in
    full)    run_full ;;
    quick)   run_quick ;;
    file)    run_file "$2" ;;
    install) run_install ;;
    clean)   run_clean ;;
    report)  run_report ;;
    help|--help|-h) print_help ;;
    *)
        echo -e "${RED}Unknown command: $1${NC}"
        print_help
        exit 1
        ;;
esac
