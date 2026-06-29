typedef char *va_list;

#define va_start(ap, last) ((ap) = (char *)__va_area__)
#define va_arg(ap, ty) (*(ty *)((ap += ((sizeof(ty) + 3) & ~3)) - ((sizeof(ty) + 3) & ~3)))
#define va_end(ap)

static int add4(int a, int b, int c, int d) {
  return a + b + c + d;
}

static int pick_second(int tag, ...) {
  va_list ap;
  va_start(ap, tag);
  int first = va_arg(ap, int);
  int second = va_arg(ap, int);
  va_end(ap);
  return first * 10 + second;
}

int main(void) {
  if (add4(1, 2, 3, 4) != 10)
    return 1;
  if (pick_second(9, 4, 7) != 47)
    return 2;
  return 0;
}
