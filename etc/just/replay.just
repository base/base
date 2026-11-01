set working-directory := '../..'

# Record transactions, replay against a fresh builder, and check diff reports
e2e:
    bash etc/scripts/devnet/replay-e2e.sh
