import os
import asyncio
import json
from pathlib import Path
from web3 import Web3
from dotenv import load_dotenv
from py_builder_relayer_client.client import RelayClient
from py_builder_relayer_client.models import SafeTransaction, OperationType
from py_builder_signing_sdk.config import BuilderConfig
from py_builder_signing_sdk.sdk_types import BuilderApiKeyCreds
from src.redeem.constants import CTF_EXCHANGE_ADDRESS, USDC_ADDRESS, CTF_ABI, RELAYER_URL


load_dotenv()


class ClaimManager:
    def __init__(self):
        self.pk = os.getenv("POLYGON_PK") or ""
        self.api_key = os.getenv("POLY_API_KEY") or ""
        self.api_secret = os.getenv("POLY_API_SECRET") or ""
        self.api_passphrase = os.getenv("POLY_API_PASSPHRASE") or ""

        # Инициализация Relayer Client (для бесплатных транзакций)
        # Используем те же ключи, что и для торговли
        creds = BuilderApiKeyCreds(
            key=self.api_key,
            secret=self.api_secret,
            passphrase=self.api_passphrase
        )
        config = BuilderConfig(local_builder_creds=creds)

        self.client = RelayClient(
            relayer_url=RELAYER_URL,
            chain_id=137,
            private_key=self.pk,
            builder_config=config
        )

        # Web3 нужен только для кодирования данных (не для отправки)
        self.w3 = Web3()

        # Преобразуем адрес в Checksum формат
        self.ctf_address = Web3.to_checksum_address(CTF_EXCHANGE_ADDRESS)
        self.usdc_address = Web3.to_checksum_address(USDC_ADDRESS)

        self.contract = self.w3.eth.contract(address=self.ctf_address, abi=CTF_ABI)

    async def claim_winnings(self, condition_id: str, winning_outcome_index: int):
        """
        Отправляет транзакцию на клейм токенов.
        winning_outcome_index: 0 для YES/UP, 1 для NO/DOWN.
        """
        print(f"🏆 Попытка забрать выигрыш для Condition: {condition_id}...")

        try:
            # 1. Подготовка данных
            # parentCollectionId для обычных рынков всегда нули
            parent_collection_id = "0x" + "0" * 64

            # IndexSets - это битовая маска.
            # Если победил исход 0 (YES/UP), маска = 1 (в двоичном 01)
            # Если победил исход 1 (NO/DOWN), маска = 2 (в двоичном 10)
            index_set = [1 << winning_outcome_index]

            # 2. Кодируем вызов функции
            encoded_data = self.contract.encode_abi(
                abi_element_identifier="redeemPositions",
                args=[
                    self.usdc_address,
                    parent_collection_id,
                    condition_id,
                    index_set
                ]
            )

            # 3. Создаем транзакцию для Relayer
            safe_tx = SafeTransaction(
                to=self.ctf_address,
                data=encoded_data,  # type: ignore
                value="0",
                operation=OperationType.Call
            )

            # 4. Отправляем через Relayer (Gasless!)
            loop = asyncio.get_running_loop()
            response = await loop.run_in_executor(None, self.client.execute, [safe_tx])

            # Достаем transactionID из ответа
            tx_id = response.transaction_id

            print(f"✅ Запрос на клейм отправлен! Task ID: {tx_id}")
            return True

        except Exception as e:
            print(f"❌ Ошибка при клейме: {e}")
            return False


def load_claim_data():
    """Загружает данные из claim.json"""
    claim_path = Path(__file__).parent / "claim.json"

    if not claim_path.exists():
        print(f"❌ Файл {claim_path} не найден!")
        print("   Убедитесь, что событие завершено и данные сохранены.")
        return None

    try:
        with open(claim_path, 'r') as f:
            data = json.load(f)

        # Проверяем наличие обязательных полей
        if "condition_id" not in data or "winning_outcome_index" not in data:
            print("❌ Некорректный формат claim.json!")
            return None

        return data
    except json.JSONDecodeError as e:
        print(f"❌ Ошибка при чтении claim.json: {e}")
        return None


async def main():
    """Entry point для клейма наград"""
    print("=== Polymarket Claim Tool ===\n")

    # Загружаем данные из claim.json
    claim_data = load_claim_data()
    if claim_data is None:
        return

    condition_id = claim_data["condition_id"]
    winning_outcome_index = claim_data["winning_outcome_index"]

    outcome_name = "UP/YES" if winning_outcome_index == 0 else "DOWN/NO"
    print(f"📋 Condition ID: {condition_id}")
    print(f"🎯 Winning Outcome: {winning_outcome_index} ({outcome_name})\n")

    # Создаем ClaimManager и выполняем клейм
    manager = ClaimManager()
    success = await manager.claim_winnings(condition_id, winning_outcome_index)

    if success:
        print("\n✅ Клейм успешно отправлен!")
        print("   Проверьте статус транзакции в вашем кошельке.")
    else:
        print("\n❌ Не удалось отправить клейм.")
        print("   Проверьте логи выше для деталей.")


if __name__ == "__main__":
    asyncio.run(main())
