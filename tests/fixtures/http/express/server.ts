import express, { Request, Response } from "express";

const app = express();

export function createOrder(req: Request, res: Response): void {
  res.status(201).end();
}

app.post("/orders", createOrder);
app.patch(`/orders/:orderId`, createOrder);
