void putc(int a);
void print(char* c,int len){
    for(int i=0;i<len;i++){
        putc(c[i]);
    }
}
int main(){
    print("hello world\n",11);
    return 0;
}