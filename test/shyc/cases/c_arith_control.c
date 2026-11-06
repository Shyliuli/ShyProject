static int fail(int code) {
  return code;
}

int main(void) {
  int sum = 0;
  for (int i = 0; i < 10; i++) {
    if ((i & 1) == 0)
      sum += i * 3 + 1;
    else
      sum -= i;
  }
  if (sum != 40)
    return fail(1);

  int n = 0;
  int i = 7;
  while (i > 0) {
    n = n * 2 + (i % 3);
    i--;
  }
  if (n != 109)
    return fail(2);

  int sw = 0;
  switch (sum / 10) {
  case 3:
    sw = 1;
    break;
  case 4:
    sw = 42;
    break;
  default:
    sw = 2;
  }
  if (sw != 42)
    return fail(3);

  int neg = -1;
  if (!(neg < 0))
    return fail(4);
  if (!(neg <= 0))
    return fail(5);
  if (0 < neg)
    return fail(6);

  unsigned int u = (unsigned int)-1;
  if (u < 0)
    return fail(7);
  if (!(1u < u))
    return fail(8);

  return 0;
}
