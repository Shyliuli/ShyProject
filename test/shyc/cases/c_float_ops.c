int main(void) {
  float a = 1.5f;
  float b = 2.25f;
  double c = 4.0;
  double d = 0.5;

  if (!((a + b) > 3.7f && (a + b) < 3.8f))
    return 1;
  if (!((c + d) > 4.4 && (c + d) < 4.6))
    return 2;
  if ((int)(a * 4.0f) != 6)
    return 3;

  return 0;
}
