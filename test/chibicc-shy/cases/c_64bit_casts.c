int main(void) {
  unsigned long a = 0xffffffffUL;
  unsigned long b = a + 2;
  if (b != 0x100000001UL)
    return 1;

  long c = -5;
  if ((c >> 1) != -3)
    return 2;

  if ((int)(char)0x100 != 0)
    return 3;
  if ((int)(signed char)0xff != -1)
    return 4;
  if ((unsigned short)0x12345 != 0x2345)
    return 5;

  return 0;
}
