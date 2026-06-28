run: main.sfs
	shyemu ./main.sfs
main.sfs: main.shyc
	shycc ./main.shyc -S ./main.shy
	shycc ./main.shyc -llibshy -o ./main.sfs --sym ./main.sym
