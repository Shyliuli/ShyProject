void putc(int a){
    asm!(a){
        "oututfa {a}"
    };
}
