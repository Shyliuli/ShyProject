struct Pair {
  int a;
  int b;
};

static int global_count = 7;
static char global_text[] = "shy";

static struct Pair make_pair(int a, int b) {
  struct Pair p;
  p.a = a;
  p.b = b;
  return p;
}

static int score(struct Pair *p) {
  return p->a * 10 + p->b;
}

int main(void) {
  struct Pair p = make_pair(4, 5);
  if (score(&p) != 45)
    return 1;

  global_count += p.a;
  if (global_count != 11)
    return 2;

  if (global_text[0] != 's' || global_text[1] != 'h' || global_text[2] != 'y')
    return 3;

  return 0;
}
