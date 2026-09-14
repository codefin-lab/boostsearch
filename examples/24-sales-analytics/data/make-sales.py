#!/usr/bin/env python3
"""A year of sales lines, the same every time it is run.

The generator is kept beside the file it writes so the numbers in the README can
be checked from first principles: run it, load the output into anything, and
count. The seed is fixed; change it and every number in the README changes.

    python3 data/make-sales.py > data/01-a-year-of-sales-lines-one.ndjson
"""
import datetime
import json
import random

random.seed(24)

REGIONS = ["north", "south", "east", "west", "central"]
# how busy each region is, and how generous its reps are with discounts
REGION_WEIGHT = {"north": 30, "south": 22, "east": 20, "west": 18, "central": 10}
REGION_DISCOUNT = {"north": 0.05, "south": 0.12, "east": 0.08, "west": 0.03, "central": 0.18}
CHANNELS = ["web", "store", "phone"]
CHANNEL_WEIGHT = [55, 35, 10]

# product -> (category, list price)
PRODUCTS = {
    "laptop-14": ("electronics", 899.0),
    "laptop-16": ("electronics", 1299.0),
    "monitor-27": ("electronics", 329.0),
    "headphones": ("electronics", 149.0),
    "keyboard": ("electronics", 79.0),
    "mouse": ("electronics", 29.0),
    "sofa": ("home", 1150.0),
    "lamp": ("home", 45.0),
    "rug": ("home", 210.0),
    "kettle": ("home", 39.0),
    "blender": ("home", 89.0),
    "mower": ("garden", 420.0),
    "hose": ("garden", 35.0),
    "shears": ("garden", 24.0),
    "planter": ("garden", 18.0),
    "novel": ("books", 14.0),
    "cookbook": ("books", 32.0),
    "atlas": ("books", 55.0),
}
PRODUCT_WEIGHT = [6, 3, 5, 9, 10, 14, 1, 10, 3, 9, 6, 2, 8, 9, 10, 16, 7, 3]
# sold a handful of times all year: what `rare_terms` exists to find
RARE = [("telescope", "electronics", 740.0, 1), ("hammock", "garden", 130.0, 2),
        ("globe", "books", 95.0, 1)]
REPS = {r: [f"{r}-rep-{i}" for i in range(1, 4)] for r in REGIONS}
# November and December sell more; the shape auto_date_histogram has to find
MONTH_WEIGHT = [7, 6, 7, 7, 8, 8, 7, 7, 8, 9, 13, 16]

start = datetime.datetime(2025, 1, 1)


def a_day():
    month = random.choices(range(1, 13), MONTH_WEIGHT)[0]
    days = (datetime.date(2025 + (month == 12), month % 12 + 1, 1)
            - datetime.date(2025, month, 1)).days
    return datetime.datetime(2025, month, random.randint(1, days),
                             random.randint(8, 21), random.randint(0, 59))


lines = []
n = 0


def sale(product, category, price, region=None):
    global n
    n += 1
    region = region or random.choices(REGIONS, [REGION_WEIGHT[r] for r in REGIONS])[0]
    qty = random.choices([1, 2, 3, 4, 5, 10, 20], [50, 20, 10, 6, 6, 5, 3])[0]
    unit = round(price * random.uniform(0.95, 1.05), 2)
    disc = round(max(0.0, min(0.4, random.gauss(REGION_DISCOUNT[region], 0.04))), 2)
    revenue = round(qty * unit * (1 - disc), 2)
    cost = round(qty * price * 0.62, 2)
    doc = {
        "order_id": f"SO-{n:05d}",
        "sold_at": a_day().strftime("%Y-%m-%dT%H:%M:00Z"),
        "region": region,
        "rep": random.choice(REPS[region]),
        "channel": random.choices(CHANNELS, CHANNEL_WEIGHT)[0],
        "category": category,
        "product": product,
        "quantity": qty,
        "unit_price": unit,
        "discount": disc,
        "revenue": revenue,
        "cost": cost,
    }
    # one sale in five was never rated; `missing` has to be told what that means
    if random.random() < 0.8:
        doc["rating"] = random.choices([1, 2, 3, 4, 5], [4, 6, 15, 40, 35])[0]
    # coupons are rare, and a document without one has no field at all
    if random.random() < 0.15:
        doc["coupon"] = random.choice(["WELCOME10", "BLACKFRIDAY", "LOYALTY"])
    lines.append(json.dumps({"index": {"_id": doc["order_id"]}}))
    lines.append(json.dumps(doc, sort_keys=False))


names = list(PRODUCTS)
for _ in range(3000):
    p = random.choices(names, PRODUCT_WEIGHT)[0]
    sale(p, *PRODUCTS[p])
for product, category, price, times in RARE:
    for _ in range(times):
        sale(product, category, price)

print("\n".join(lines))
