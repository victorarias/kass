import { describe, expect, it, vi } from 'vitest';
import { Cart, type Prices } from './cart';

const fixedPrices = (table: Record<string, number>): Prices => ({
  priceOf: async (sku) => table[sku],
});

describe('Cart', () => {
  it('sums quantity times price', async () => {
    const cart = new Cart(fixedPrices({ apple: 2, pear: 3 }));
    cart.add('apple', 2);
    cart.add('pear');
    expect(await cart.total()).toBe(7);
  });

  it('asks for the price of each item', async () => {
    const prices = { priceOf: vi.fn().mockResolvedValue(5) };
    const cart = new Cart(prices);
    cart.add('apple');
    await cart.total();
    expect(prices.priceOf).toHaveBeenCalledWith('apple');
  });

  it('totals an empty cart', async () => {
    const cart = new Cart(fixedPrices({}));
    await cart.total();
  });
});
