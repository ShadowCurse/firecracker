N=${1:-100}

sudo mkdir -p /srv/mounts

for m in $(ls /srv/mounts || true); do
  echo Unmounting /srv/mounts/$m
  sudo umount /srv/mounts/$m
  sudo rm -rf /srv/mounts/$m
done

for i in $(seq $N); do
  echo Mounting /srv/mounts/mount-$i
  sudo mkdir /srv/mounts/mount-$i
  sudo mount --bind /srv/mounts/mount-$i /srv/mounts/mount-$i
done
