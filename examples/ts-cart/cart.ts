export interface Prices {
  priceOf(sku: string): Promise<number>;
}

export class Cart {
  private items = new Map<string, number>();

  constructor(private prices: Prices) {}

  add(sku: string, qty = 1) {
    this.items.set(sku, (this.items.get(sku) ?? 0) + qty);
  }

  async total(): Promise<number> {
    let sum = 0;
    for (const [sku, qty] of this.items) sum += (await this.prices.priceOf(sku)) * qty;
    return sum;
  }
}
