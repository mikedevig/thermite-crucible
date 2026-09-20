import random
random.seed(42)
seeds = [random.randint(1, 2**32 - 1) for _ in range(100)]
with open("seeds.txt", "w") as f:
    for s in seeds:
        f.write(f"{s}\n")
print(f"Generated {len(seeds)} seeds -> seeds.txt")
