JAIL_BIN=$1
N=${2:-100}
RESULT_DIR=$3

bash make_mounts.sh $N

rm -rf $RESULT_DIR
mkdir -p $RESULT_DIR

for i in $(seq 100); do
  sudo rm -rf /srv/jailer/*
  sudo strace -cw $JAIL_BIN --exec-file jailer_time --gid 69 --uid 69 --id 69 --chroot-base-dir /srv/jailer &> $RESULT_DIR/$i
done
