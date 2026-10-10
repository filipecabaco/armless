echo "arith $((123456789 * 987654321 % 1000003))"
awk 'BEGIN { s=0; for (i=1;i<=1000;i++) s+=sqrt(i); printf "awk %.6f\n", s }'
seq 1 2000 | sort -rn | head -3 | tr '\n' ' '; echo
echo "md5 $(seq 1 5000 | md5sum)"
echo "gz $(seq 1 20000 | gzip -9 | gunzip | sha256sum)"
echo hello world | sed 's/o/0/g' | tr a-z A-Z
printf '%s\n' banana apple cherry apple | sort | uniq -c | tr -s ' ' | tr '\n' ';'; echo
echo "dc $(dc -e '2 64 ^ p')"
echo "xxd $(echo -n armless | xxd -p)"
i=0; while [ $i -lt 300 ]; do i=$((i+1)); done; echo "loop $i"
