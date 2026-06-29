static int sum_array(int *p, int n) {
  int sum = 0;
  for (int i = 0; i < n; i++)
    sum += p[i];
  return sum;
}

int main(void) {
  int xs[6];
  for (int i = 0; i < 6; i++)
    xs[i] = i + 1;

  if (sum_array(xs, 6) != 21)
    return 1;

  int *p = xs + 2;
  p[1] = 30;
  if (xs[3] != 30)
    return 2;

  char buf[5] = {'s', 'h', 'y', '!', 0};
  if (buf[0] != 's' || *(buf + 2) != 'y')
    return 3;

  return 0;
}
